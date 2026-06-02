//! Minimal DWARF debug info for native builds (`docs/25`): a `.debug_line`
//! line-number program plus a compile-unit `.debug_info`, so debuggers (gdb/
//! lldb) can map machine addresses back to source lines.
//!
//! The codegen tags each instruction with its source byte offset (`set_srcloc`),
//! captured per function as `(FuncId, code_offset, source_offset)` ranges
//! (`Codegen.line_info`) plus each function's code length. Here we turn those
//! into a DWARF line program: one sequence per function whose rows map a
//! function-relative code offset to a source line. Function start addresses are
//! emitted as `Address::Symbol` relocations against each function's object
//! symbol, so the linker fixes them up.

use std::collections::HashMap;

use cranelift_module::FuncId;
use cranelift_object::ObjectProduct;
use cranelift_object::object::write::{Relocation, RelocationFlags};
use cranelift_object::object::{BinaryFormat, RelocationEncoding, RelocationKind, SectionKind};
use gimli::write::{
    Address, AttributeValue, DwarfUnit, EndianVec, LineProgram, LineString, Sections, Writer,
};
use gimli::{Encoding, Format, LineEncoding, RunTimeEndian};

/// The 1-based line number of `byte_offset` within `src` (newline-counting).
/// Out-of-range offsets clamp to the last line. Used to fill line-program rows.
pub(crate) fn byte_to_line(src: &str, byte_offset: u32) -> u32 {
    let off = (byte_offset as usize).min(src.len());
    1 + src.as_bytes()[..off]
        .iter()
        .filter(|&&b| b == b'\n')
        .count() as u32
}

/// A relocation gimli asked us to record while serializing a debug section:
/// either an absolute code address (a function symbol) or a reference to the
/// start of another debug section.
#[derive(Clone)]
struct PendingReloc {
    offset: usize,
    size: u8,
    target: RelocTarget,
    addend: i64,
}

#[derive(Clone)]
enum RelocTarget {
    /// Index into the caller's symbol table (a function's object symbol).
    Symbol(usize),
}

/// A gimli `Writer` over an in-memory buffer that records the relocations a
/// relocatable object needs (addresses + cross-section offsets), writing zero
/// placeholders that the object's relocations later fix up.
#[derive(Clone)]
struct DwarfWriter {
    vec: EndianVec<RunTimeEndian>,
    relocs: Vec<PendingReloc>,
}

impl DwarfWriter {
    fn new(endian: RunTimeEndian) -> Self {
        DwarfWriter {
            vec: EndianVec::new(endian),
            relocs: Vec::new(),
        }
    }
}

impl Writer for DwarfWriter {
    type Endian = RunTimeEndian;
    fn endian(&self) -> RunTimeEndian {
        self.vec.endian()
    }
    fn len(&self) -> usize {
        self.vec.len()
    }
    fn write(&mut self, bytes: &[u8]) -> gimli::write::Result<()> {
        self.vec.write(bytes)
    }
    fn write_at(&mut self, offset: usize, bytes: &[u8]) -> gimli::write::Result<()> {
        self.vec.write_at(offset, bytes)
    }
    fn write_address(&mut self, address: Address, size: u8) -> gimli::write::Result<()> {
        match address {
            Address::Constant(val) => self.write_udata(val, size),
            Address::Symbol { symbol, addend } => {
                self.relocs.push(PendingReloc {
                    offset: self.len(),
                    size,
                    target: RelocTarget::Symbol(symbol),
                    addend,
                });
                self.write_udata(0, size)
            }
        }
    }
    // `write_offset`/`write_offset_at` use gimli's defaults (write the literal
    // section-relative value): DWARF cross-section references are self-contained
    // within this single object, so no relocation is needed (and a 32-bit
    // absolute relocation is rejected by `ld64` on 64-bit Mach-O anyway). Only
    // code addresses (`write_address`, 8 bytes) are relocated against symbols.
}

/// Build the DWARF line program + compile unit from the captured per-function
/// source-line ranges and serialize them into `product.object` (`.debug_line`,
/// `.debug_info`, `.debug_abbrev`, `.debug_str`), with relocations against each
/// function's symbol. `src`/`file_name` are the (single) source file.
pub(crate) fn emit_dwarf(
    product: &mut ObjectProduct,
    line_info: &[(FuncId, u32, u32)],
    func_len: &HashMap<FuncId, u32>,
    src: &str,
    file_name: &str,
) -> Result<(), gimli::write::Error> {
    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };
    let mut dwarf = DwarfUnit::new(encoding);

    let comp_dir = LineString::String(b".".to_vec());
    let comp_file = LineString::String(file_name.as_bytes().to_vec());
    let mut program = LineProgram::new(
        encoding,
        LineEncoding::default(),
        comp_dir.clone(),
        comp_file.clone(),
        None,
    );

    // Group the captured rows by function, preserving order.
    let mut by_func: Vec<(FuncId, Vec<(u32, u32)>)> = Vec::new();
    for &(func, code_off, src_off) in line_info {
        match by_func.iter_mut().find(|(f, _)| *f == func) {
            Some((_, rows)) => rows.push((code_off, src_off)),
            None => by_func.push((func, vec![(code_off, src_off)])),
        }
    }

    // Assign a gimli "symbol index" to each function, in first-seen order; the
    // serialized relocations resolve these to the functions' object symbols.
    let mut func_syms: Vec<FuncId> = Vec::new();
    let file_id = program.add_file(comp_file, program.default_directory(), None);

    for (func, rows) in &by_func {
        let sym = func_syms.len();
        func_syms.push(*func);
        let len = func_len.get(func).copied().unwrap_or(0) as u64;
        program.begin_sequence(Some(Address::Symbol {
            symbol: sym,
            addend: 0,
        }));
        for &(code_off, src_off) in rows {
            let row = program.row();
            row.address_offset = code_off as u64;
            row.file = file_id;
            row.line = byte_to_line(src, src_off) as u64;
            program.generate_row();
        }
        program.end_sequence(len);
    }

    dwarf.unit.line_program = program;
    // A minimal compile-unit root DIE referencing the line program.
    let name = dwarf.strings.add(file_name);
    let dir = dwarf.strings.add(".");
    let root = dwarf.unit.root();
    let root_die = dwarf.unit.get_mut(root);
    root_die.set(gimli::DW_AT_name, AttributeValue::StringRef(name));
    root_die.set(gimli::DW_AT_comp_dir, AttributeValue::StringRef(dir));
    root_die.set(
        gimli::DW_AT_producer,
        AttributeValue::String(b"otter_fusion".to_vec()),
    );

    // Serialize all sections through our relocation-recording writer.
    let mut sections = Sections::new(DwarfWriter::new(RunTimeEndian::Little));
    dwarf.write(&mut sections)?;

    // Add each non-empty section to the object, then attach its address
    // relocations (function-symbol references) — 8-byte absolutes, valid on both
    // ELF and 64-bit Mach-O. Section placement is format-specific: ELF uses the
    // `.debug_*` names with no segment; Mach-O puts `__debug_*` sections in the
    // `__DWARF` segment (which `ld64` special-cases for debug relocs/alignment).
    let macho = product.object.format() == BinaryFormat::MachO;
    sections.for_each(|id, w| -> Result<(), gimli::write::Error> {
        if w.vec.len() == 0 {
            return Ok(());
        }
        let (seg, name) = if macho {
            (
                b"__DWARF".to_vec(),
                format!("__{}", id.name().trim_start_matches('.')).into_bytes(),
            )
        } else {
            (Vec::new(), id.name().as_bytes().to_vec())
        };
        let obj_sec = product.object.add_section(seg, name, SectionKind::Debug);
        product
            .object
            .section_mut(obj_sec)
            .set_data(w.vec.clone().into_vec(), 1);
        for r in &w.relocs {
            let RelocTarget::Symbol(i) = r.target;
            let Some((symbol, _)) = product.functions[func_syms[i]] else {
                continue;
            };
            let _ = product.object.add_relocation(
                obj_sec,
                Relocation {
                    offset: r.offset as u64,
                    symbol,
                    addend: r.addend,
                    flags: RelocationFlags::Generic {
                        kind: RelocationKind::Absolute,
                        encoding: RelocationEncoding::Generic,
                        size: r.size * 8,
                    },
                },
            );
        }
        Ok(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::byte_to_line;

    #[test]
    fn byte_to_line_counts_newlines() {
        let src = "a\nbb\nccc\n";
        assert_eq!(byte_to_line(src, 0), 1);
        assert_eq!(byte_to_line(src, 2), 2); // first byte of line 2
        assert_eq!(byte_to_line(src, 5), 3);
        assert_eq!(byte_to_line(src, 999), 4); // clamps past end
    }
}
