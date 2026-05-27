    use super::*;
    use compiler::lexer::lex;
    use compiler::parser::parse;
    use compiler::sema::analyze;
    use compiler::span::FileId;

    /// Analyze, JIT-compile, and call a zero-arg `i64` function by name.
    fn run(src: &str, func: &str) -> i64 {
        let (tokens, le) = lex(src, FileId(0));
        assert!(le.is_empty(), "lex: {le:?}");
        let (module, pe) = parse(src, &tokens);
        assert!(pe.is_empty(), "parse: {pe:?}");
        let analysis = analyze(&module);
        assert!(analysis.errors.is_empty(), "sema: {:?}", analysis.errors);
        let jit = compile(&analysis).expect("codegen");
        unsafe { jit.call_i64(func).expect("function present") }
    }

    /// Call a zero-arg function returning `str` and read back its UTF-8 bytes.
    fn run_str(src: &str, func: &str) -> String {
        let (tokens, le) = lex(src, FileId(0));
        assert!(le.is_empty(), "lex: {le:?}");
        let (module, pe) = parse(src, &tokens);
        assert!(pe.is_empty(), "parse: {pe:?}");
        let analysis = analyze(&module);
        assert!(analysis.errors.is_empty(), "sema: {:?}", analysis.errors);
        let jit = compile(&analysis).expect("codegen");
        let bits = unsafe { jit.call_i64(func).expect("function present") };
        let p = bits as usize as *const runtime::LangStr;
        unsafe { String::from_utf8_lossy(runtime::str_bytes(p)).into_owned() }
    }

    #[test]
    fn returns_constant() {
        assert_eq!(run("function answer(): i64 { 42 }", "answer"), 42);
    }

    #[test]
    fn arithmetic() {
        assert_eq!(run("function f(): i64 { 40 + 2 }", "f"), 42);
        assert_eq!(run("function f(): i64 { (6 - 2) * 10 + 2 }", "f"), 42);
        assert_eq!(run("function f(): i64 { 84 / 2 }", "f"), 42);
        assert_eq!(run("function f(): i64 { 85 % 43 }", "f"), 42);
    }

    #[test]
    fn locals_and_assignment() {
        assert_eq!(
            run("function f(): i64 { var x: i64 = 40; x = x + 2; x }", "f"),
            42
        );
    }

    #[test]
    fn shadowing_distinct_locals() {
        assert_eq!(
            run("function f(): i64 { var x: i64 = 1; var y: i64 = { var x: i64 = 40; x + 1 }; x + y + 0 }", "f"),
            42
        );
    }

    #[test]
    fn if_else_value() {
        assert_eq!(
            run("function f(): i64 { if 1 < 2 { 42 } else { 0 } }", "f"),
            42
        );
        assert_eq!(
            run("function f(): i64 { if 2 < 1 { 0 } else { 42 } }", "f"),
            42
        );
    }

    #[test]
    fn early_return() {
        assert_eq!(
            run("function f(): i64 { if 1 < 2 { return 42 } 7 }", "f"),
            42
        );
    }

    #[test]
    fn logical_short_circuit() {
        // (true && true) -> branch picks 42
        assert_eq!(
            run("function f(): i64 { if (1 < 2) && (3 < 4) { 42 } else { 0 } }", "f"),
            42
        );
        assert_eq!(
            run("function f(): i64 { if (1 < 2) || (4 < 3) { 42 } else { 0 } }", "f"),
            42
        );
    }

    #[test]
    fn calls_other_function() {
        assert_eq!(
            run("function add(a: i64, b: i64): i64 { a + b }\nfunction f(): i64 { add(40, 2) }", "f"),
            42
        );
    }

    #[test]
    fn recursion_factorial() {
        let src = "function fac(n: i64): i64 { if n <= 1 { 1 } else { n * fac(n - 1) } }\n\
                   function f(): i64 { fac(5) }";
        assert_eq!(run(src, "f"), 120);
    }

    #[test]
    fn narrower_int_width() {
        // i32 arithmetic returns correctly through the i32 ABI then widened.
        assert_eq!(run("function f(): i32 { 40i32 + 2i32 }", "f") as i32, 42);
    }

    #[test]
    fn bitwise_and_shifts() {
        assert_eq!(run("function f(): i64 { 1 << 5 }", "f"), 32);
        assert_eq!(run("function f(): i64 { 0xFF & 0x0F }", "f"), 15);
        assert_eq!(run("function f(): i64 { 5 ^ 6 }", "f"), 3);
    }

    // --- strings -----------------------------------------------------------

    #[test]
    fn string_literal_roundtrip() {
        assert_eq!(run_str("function f(): str { \"hello\" }", "f"), "hello");
        assert_eq!(run_str("function f(): str { \"\" }", "f"), "");
    }

    #[test]
    fn string_escapes() {
        assert_eq!(run_str("function f(): str { \"a\\nb\\tc\" }", "f"), "a\nb\tc");
        assert_eq!(run_str("function f(): str { \"quote: \\\"x\\\"\" }", "f"), "quote: \"x\"");
    }

    #[test]
    fn string_concat() {
        assert_eq!(run_str("function f(): str { \"foo\" + \"bar\" }", "f"), "foobar");
        assert_eq!(
            run_str("function f(): str { \"a\" + \"b\" + \"c\" + \"d\" }", "f"),
            "abcd"
        );
    }

    #[test]
    fn int_to_str() {
        assert_eq!(run_str("function f(): str { 42 as str }", "f"), "42");
        assert_eq!(run_str("function f(): str { (0 - 7) as str }", "f"), "-7");
        assert_eq!(run_str("function f(): str { 255u8 as str }", "f"), "255");
    }

    #[test]
    fn float_bool_char_to_str() {
        assert_eq!(run_str("function f(): str { 3.5 as str }", "f"), "3.5");
        assert_eq!(run_str("function f(): str { true as str }", "f"), "true");
        assert_eq!(run_str("function f(): str { false as str }", "f"), "false");
        assert_eq!(run_str("function f(): str { 'A' as str }", "f"), "A");
    }

    #[test]
    fn concat_with_stringified_number() {
        assert_eq!(
            run_str("function f(): str { \"n = \" + (42 as str) }", "f"),
            "n = 42"
        );
    }

    // --- numeric conversions (cast) ---------------------------------------

    #[test]
    fn int_widen_and_narrow() {
        // i32 -> i64 widening preserves value.
        assert_eq!(run("function f(): i64 { var x: i32 = 300; x as i64 }", "f"), 300);
        // i64 -> i8 truncation, then back to i64 sign-extended.
        assert_eq!(
            run("function f(): i64 { var x: i64 = 300; (x as i8) as i64 }", "f"),
            300i64 as i8 as i64
        );
    }

    #[test]
    fn float_to_int_truncates() {
        assert_eq!(run("function f(): i64 { 3.9 as i64 }", "f"), 3);
        assert_eq!(run("function f(): i64 { (0.0 - 3.9) as i64 }", "f"), -3);
    }

    #[test]
    fn int_to_float_roundtrip() {
        // 7 -> f64 -> i64 == 7
        assert_eq!(run("function f(): i64 { (7 as f64) as i64 }", "f"), 7);
    }

    #[test]
    fn char_int_roundtrip() {
        assert_eq!(run("function f(): i64 { 'A' as i64 }", "f"), 65);
        // 66 -> char -> i64 == 66
        assert_eq!(run("function f(): i64 { (66 as char) as i64 }", "f"), 66);
    }

    // --- loops -------------------------------------------------------------

    #[test]
    fn while_sum() {
        let src = "function f(): i64 {\n\
                     var i: i64 = 0;\n\
                     var total: i64 = 0;\n\
                     while i < 10 { total = total + i; i = i + 1; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 45);
    }

    #[test]
    fn while_with_break_and_continue() {
        // Sum even numbers below 10, stop at 100 (never reached): 0+2+4+6+8 = 20
        let src = "function f(): i64 {\n\
                     var i: i64 = 0;\n\
                     var total: i64 = 0;\n\
                     while i < 10 {\n\
                       i = i + 1;\n\
                       if (i - 1) % 2 == 1 { continue }\n\
                       total = total + (i - 1);\n\
                     }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 20);
    }

    #[test]
    fn loop_breaks_with_value() {
        let src = "function f(): i64 {\n\
                     var i: i64 = 0;\n\
                     loop {\n\
                       if i >= 42 { break i }\n\
                       i = i + 1;\n\
                     }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn loop_with_plain_break() {
        let src = "function f(): i64 {\n\
                     var i: i64 = 0;\n\
                     var n: i64 = 0;\n\
                     loop {\n\
                       n = n + i;\n\
                       i = i + 1;\n\
                       if i > 5 { break }\n\
                     }\n\
                     n\n\
                   }";
        // i runs 0..=5 accumulating before the post-increment check: 0+1+2+3+4+5 = 15
        assert_eq!(run(src, "f"), 15);
    }

    // --- structs -----------------------------------------------------------

    #[test]
    fn record_struct_construct_and_access() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function f(): i64 { var p = P { x: 40, y: 2 }; p.x + p.y }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn record_field_mutation() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function f(): i64 { var p = P { x: 1, y: 2 }; p.x = 40; p.x + p.y }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn field_init_shorthand() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function f(): i64 { var x: i64 = 40; var y: i64 = 2; var p = P { x, y }; p.x + p.y }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn struct_spread_update() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function f(): i64 { var p = P { x: 1, y: 2 }; var q = P { ..p, y: 41 }; q.x + q.y }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn nested_structs() {
        let src = "struct Inner { v: i64 }\n\
                   struct Outer { inner: Inner, k: i64 }\n\
                   function f(): i64 {\n\
                     var o = Outer { inner: Inner { v: 40 }, k: 2 };\n\
                     o.inner.v + o.k\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_struct_construct_and_index() {
        let src = "struct Pair(i64, i64)\n\
                   function f(): i64 { var p = Pair(40, 2); p.0 + p.1 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn struct_with_string_field() {
        let src = "struct Person { name: str, age: i64 }\n\
                   function f(): str { var p = Person { name: \"Alice\", age: 30 }; p.name }";
        assert_eq!(run_str(src, "f"), "Alice");
    }

    #[test]
    fn mixed_width_field_layout() {
        // a: u8 (off 0), b: i32 (off 4 after padding), c: i64 (off 8) — verifies
        // alignment-respecting offsets by reading each field back.
        let src = "struct M { a: u8, b: i32, c: i64 }\n\
                   function f(): i64 {\n\
                     var m = M { a: 7u8, b: 1000i32, c: 9000000000 };\n\
                     (m.a as i64) + (m.b as i64) + m.c\n\
                   }";
        assert_eq!(run(src, "f"), 7 + 1000 + 9_000_000_000);
    }

    #[test]
    fn struct_passed_to_function() {
        let src = "struct P { x: i64, y: i64 }\n\
                   function sum(p: P): i64 { p.x + p.y }\n\
                   function f(): i64 { sum(P { x: 40, y: 2 }) }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- operator overloading ----------------------------------------------

    #[test]
    fn overloaded_add_on_struct() {
        let src = "struct V { x: i64, y: i64 }\n\
                   extend V: Add { function add(self, rhs: V): V { V { x: self.x + rhs.x, y: self.y + rhs.y } } }\n\
                   function f(): i64 { var c = V { x: 40, y: 1 } + V { x: 2, y: 99 }; c.x }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn overloaded_eq_on_struct() {
        let src = "struct P { v: i64 }\n\
                   extend P: Eq { function eq(self, o: P): bool { self.v == o.v } }\n\
                   function f(): i64 {\n\
                     var a = P { v: 5 };\n\
                     if a == (P { v: 5 }) { 42 } else { 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn overloaded_ne_negates_eq() {
        let src = "struct P { v: i64 }\n\
                   extend P: Eq { function eq(self, o: P): bool { self.v == o.v } }\n\
                   function f(): i64 { var a = P { v: 5 }; if a != (P { v: 9 }) { 42 } else { 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn overloaded_lt_on_struct() {
        let src = "struct M { n: i64 }\n\
                   extend M: Ord { function lt(self, o: M): bool { self.n < o.n } }\n\
                   function f(): i64 { if (M { n: 1 }) < (M { n: 2 }) { 42 } else { 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- List<T> -----------------------------------------------------------

    #[test]
    fn list_literal_and_index() {
        let src = "function f(): i64 { var xs: List<i64> = [10, 20, 12]; xs[0] + xs[2] }";
        assert_eq!(run(src, "f"), 22);
    }

    #[test]
    fn list_push_and_size() {
        let src = "function f(): i64 {\n\
                     var xs = [1, 2];\n\
                     xs.push(3);\n\
                     xs.push(4);\n\
                     xs.size()\n\
                   }";
        assert_eq!(run(src, "f"), 4);
    }

    #[test]
    fn list_sum_with_while() {
        let src = "function f(): i64 {\n\
                     var xs = [1, 2, 3, 4, 5, 6, 7, 8, 9];\n\
                     var i: i64 = 0;\n\
                     var total: i64 = 0;\n\
                     while i < xs.size() { total = total + xs[i]; i = i + 1; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 45);
    }

    #[test]
    fn list_indexed_assignment() {
        let src = "function f(): i64 { var xs = [1, 2, 3]; xs[1] = 40; xs[0] + xs[1] + xs[2] }";
        assert_eq!(run(src, "f"), 44);
    }

    #[test]
    fn list_of_strings() {
        let src = "function f(): str { var xs: List<str> = [\"a\", \"b\", \"c\"]; xs[1] }";
        assert_eq!(run_str(src, "f"), "b");
    }

    #[test]
    fn list_of_structs() {
        let src = "struct P { v: i64 }\n\
                   function f(): i64 { var xs = [P { v: 40 }, P { v: 2 }]; xs[0].v + xs[1].v }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn list_get_in_range() {
        let src = "function f(): i64 {\n\
                     var xs = [10, 42, 30];\n\
                     match xs.get(1) { i64 n => n, null => 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn list_get_out_of_range_is_null() {
        let src = "function f(): i64 {\n\
                     var xs = [10, 20];\n\
                     match xs.get(5) { i64 n => n, null => 42 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn list_get_str_element() {
        let src = "function f(): str {\n\
                     var xs: List<str> = [\"a\", \"hit\", \"c\"];\n\
                     match xs.get(1) { str s => s, null => \"miss\" }\n\
                   }";
        assert_eq!(run_str(src, "f"), "hit");
    }

    #[test]
    fn list_is_empty() {
        let src = "function f(): i64 {\n\
                     var xs: List<i64> = [];\n\
                     if xs.is_empty() { 42 } else { 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    // -- Map<K, V> -----------------------------------------------------------

    #[test]
    fn map_literal_get_str_keys() {
        let src = "function f(): i64 {\n\
                     var m: Map<str, i64> = { \"x\": 1, \"y\": 42 };\n\
                     match m.get(\"y\") { i64 n => n, null => 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_get_missing_is_null() {
        let src = "function f(): i64 {\n\
                     var m: Map<str, i64> = { \"x\": 1 };\n\
                     match m.get(\"nope\") { i64 n => n, null => 42 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_set_overwrites_and_size() {
        let src = "function f(): i64 {\n\
                     var m: Map<str, i64> = { \"a\": 1 };\n\
                     m.set(\"a\", 10);\n\
                     m.set(\"b\", 32);\n\
                     match m.get(\"a\") { i64 n => n + m.size(), null => 0 }\n\
                   }";
        // a=10 (overwritten, not added) + size 2 (a,b) = ... wait: 10 + 2 = 12
        assert_eq!(run(src, "f"), 12);
    }

    #[test]
    fn map_int_keys() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 100, 2: 200 };\n\
                     m.set(3, 300);\n\
                     match m.get(3) { i64 n => n, null => 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 300);
    }

    #[test]
    fn map_contains_and_remove() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 10, 2: 20 };\n\
                     var before = if m.contains(1) { 1 } else { 0 };\n\
                     m.remove(1);\n\
                     var after = if m.contains(1) { 1 } else { 0 };\n\
                     before * 100 + after * 10 + m.size()\n\
                   }";
        // before=1 -> 100, after=0 -> 0, size=1 -> 1 => 101
        assert_eq!(run(src, "f"), 101);
    }

    #[test]
    fn map_remove_returns_value() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 5: 42 };\n\
                     match m.remove(5) { i64 n => n, null => -1 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_remove_missing_is_null() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 5: 42 };\n\
                     match m.remove(9) { i64 n => n, null => 7 }\n\
                   }";
        assert_eq!(run(src, "f"), 7);
    }

    #[test]
    fn map_empty_constructor_is_empty() {
        let src = "function f(): i64 {\n\
                     var m = Map<str, i64>();\n\
                     if m.is_empty() { 42 } else { 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_new_constructor() {
        let src = "function f(): i64 {\n\
                     var m = Map.new<i64, i64>();\n\
                     m.set(1, 42);\n\
                     match m.get(1) { i64 n => n, null => 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_str_values() {
        let src = "function f(): str {\n\
                     var m: Map<i64, str> = { 1: \"one\", 2: \"two\" };\n\
                     match m.get(2) { str s => s, null => \"?\" }\n\
                   }";
        assert_eq!(run_str(src, "f"), "two");
    }

    #[test]
    fn map_keys_sum() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 10: 1, 20: 2, 12: 3 };\n\
                     var total = 0;\n\
                     for k in m.keys() { total = total + k; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_values_sum() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 10, 2: 20, 3: 12 };\n\
                     var total = 0;\n\
                     for v in m.values() { total = total + v; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_clear() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 1, 2: 2, 3: 3 };\n\
                     m.clear();\n\
                     if m.is_empty() { 42 } else { m.size() }\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn map_literal_spread_rightmost_wins() {
        let src = "function f(): i64 {\n\
                     var a: Map<str, i64> = { \"p\": 1, \"q\": 2 };\n\
                     var b: Map<str, i64> = { \"q\": 40 };\n\
                     var c: Map<str, i64> = { ..a, ..b, \"r\": 1 };\n\
                     var q = match c.get(\"q\") { i64 n => n, null => 0 };\n\
                     q + c.size()\n\
                   }";
        // q = 40 (b wins), size = 3 (p, q, r) => 43
        assert_eq!(run(src, "f"), 43);
    }

    #[test]
    fn map_grows_past_initial_capacity() {
        let src = "function f(): i64 {\n\
                     var m = Map.new<i64, i64>();\n\
                     var i = 0;\n\
                     while i < 50 { m.set(i, i * 2); i = i + 1; }\n\
                     match m.get(40) { i64 n => n + m.size(), null => 0 }\n\
                   }";
        // get(40) = 80, size = 50 => 130
        assert_eq!(run(src, "f"), 130);
    }

    // -- record-struct generic inference & the Iterator protocol -------------

    const RANGE_SRC: &str = "\
struct Range { current: i64, end: i64 }\n\
extend Range: Iterator<i64> {\n\
  function next(self): Item<i64> | Done {\n\
    if self.current >= self.end { Done {} }\n\
    else { var v = self.current; self.current = self.current + 1; Item { value: v } }\n\
  }\n\
}\n";

    #[test]
    fn record_struct_generic_arg_inferred_from_field() {
        // `Box { item: 42 }` infers `Box<i64>` from the field value.
        let src = "struct Box<T> { item: T }\n\
                   function f(): i64 { var b = Box { item: 42 }; b.item }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn for_over_iterator_sums() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var total = 0;\n\
               for x in (Range {{ current: 1, end: 5 }}) {{ total = total + x; }}\n\
               total\n\
             }}"
        );
        // 1 + 2 + 3 + 4 = 10
        assert_eq!(run(&src, "f"), 10);
    }

    #[test]
    fn for_over_iterator_break_continue() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var sum = 0;\n\
               var r = Range {{ current: 0, end: 100 }};\n\
               for n in r {{\n\
                 if n == 2 {{ continue; }}\n\
                 if n == 5 {{ break; }}\n\
                 sum = sum + n;\n\
               }}\n\
               sum\n\
             }}"
        );
        // 0 + 1 + (skip 2) + 3 + 4 + (break at 5) = 8
        assert_eq!(run(&src, "f"), 8);
    }

    #[test]
    fn iterator_next_match_directly() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var r = Range {{ current: 7, end: 9 }};\n\
               match r.next() {{ Item<i64> it => it.value, Done d => -1 }}\n\
             }}"
        );
        assert_eq!(run(&src, "f"), 7);
    }

    #[test]
    fn iterator_done_after_exhaustion() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var r = Range {{ current: 0, end: 1 }};\n\
               match r.next() {{ Item<i64> a => 0, Done d => -1 }};\n\
               match r.next() {{ Item<i64> b => 0, Done d => 42 }}\n\
             }}"
        );
        assert_eq!(run(&src, "f"), 42);
    }

    // -- generic bounds & monomorphized interface-method dispatch ------------

    const SHOW_SRC: &str = "\
interface Show { function show(self): i64; }\n\
struct Dog {}\n\
struct Cat {}\n\
extend Dog: Show { function show(self): i64 { 1 } }\n\
extend Cat: Show { function show(self): i64 { 2 } }\n";

    #[test]
    fn bound_method_dispatch_per_instance() {
        // `tell` is monomorphized for Dog and Cat to their own `show` impls.
        let src = format!(
            "{SHOW_SRC}\
             function tell<T: Show>(x: T): i64 {{ x.show() }}\n\
             function f(): i64 {{ tell(Dog {{}}) * 10 + tell(Cat {{}}) }}"
        );
        // Dog.show=1, Cat.show=2 => 12
        assert_eq!(run(&src, "f"), 12);
    }

    #[test]
    fn bound_method_with_argument() {
        let src = "interface Adder { function add(self, n: i64): i64; }\n\
                   struct C { base: i64 }\n\
                   extend C: Adder { function add(self, n: i64): i64 { self.base + n } }\n\
                   function bump<T: Adder>(x: T, by: i64): i64 { x.add(by) }\n\
                   function f(): i64 { bump(C { base: 100 }, 23) }";
        assert_eq!(run(src, "f"), 123);
    }

    #[test]
    fn bound_generic_interface_iterator() {
        let src = format!(
            "{RANGE_SRC}\
             function first<T: Iterator<i64>>(it: T): i64 {{\n\
               match it.next() {{ Item<i64> x => x.value, Done d => -1 }}\n\
             }}\n\
             function f(): i64 {{ first(Range {{ current: 9, end: 20 }}) }}"
        );
        assert_eq!(run(&src, "f"), 9);
    }

    // -- List higher-order methods (closures + trailing-closure sugar) -------

    #[test]
    fn list_map_with_trailing_closure_and_it() {
        let src = "function f(): i64 {\n\
                     var xs = [1, 2, 3, 4];\n\
                     var d = xs.map { it * 2 };\n\
                     d.fold(0, (a: i64, x: i64): i64 => a + x)\n\
                   }";
        // (2+4+6+8) = 20
        assert_eq!(run(src, "f"), 20);
    }

    #[test]
    fn list_filter_keeps_matching() {
        let src = "function f(): i64 {\n\
                     var xs = [1, 2, 3, 4, 5, 6];\n\
                     xs.filter { it % 2 == 0 }.size()\n\
                   }";
        assert_eq!(run(src, "f"), 3);
    }

    #[test]
    fn list_fold_sums() {
        let src = "function f(): i64 {\n\
                     [10, 20, 12].fold(0, (a: i64, x: i64): i64 => a + x)\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn list_map_changes_element_type() {
        // `map` over `List<str>` producing `List<i64>` (the lengths).
        let src = "function f(): i64 {\n\
                     var names: List<str> = [\"a\", \"bb\", \"ccc\"];\n\
                     var lens = names.map { it.size() };\n\
                     lens[0] + lens[1] + lens[2]\n\
                   }";
        // 1 + 2 + 3 = 6
        assert_eq!(run(src, "f"), 6);
    }

    #[test]
    fn list_map_explicit_closure_arg() {
        // The closure may also be passed as a normal argument (not trailing).
        let src = "function f(): i64 {\n\
                     var xs = [1, 2, 3];\n\
                     var inc = xs.map((n: i64): i64 => n + 100);\n\
                     inc[2]\n\
                   }";
        assert_eq!(run(src, "f"), 103);
    }

    // -- closures ------------------------------------------------------------

    #[test]
    fn closure_basic_call() {
        let src = "function f(): i64 { var inc = (n: i64): i64 => n + 1; inc(41) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn closure_captures_local_by_value() {
        let src = "function f(): i64 {\n\
                     var base = 100;\n\
                     var add = (n: i64): i64 => n + base;\n\
                     add(5)\n\
                   }";
        assert_eq!(run(src, "f"), 105);
    }

    #[test]
    fn closure_passed_to_function() {
        let src = "function apply(g: (i64) => i64, x: i64): i64 { g(x) }\n\
                   function f(): i64 {\n\
                     var dbl = (n: i64): i64 => n * 2;\n\
                     apply(dbl, 21)\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn closure_returned_from_function() {
        let src = "function adder(by: i64): (i64) => i64 { (n: i64): i64 => n + by }\n\
                   function f(): i64 {\n\
                     var add10 = adder(10);\n\
                     var add100 = adder(100);\n\
                     add10(5) + add100(5)\n\
                   }";
        // 15 + 105 = 120
        assert_eq!(run(src, "f"), 120);
    }

    #[test]
    fn closure_captures_str() {
        let src = "function f(): str {\n\
                     var prefix = \"hi \";\n\
                     var greet = (name: str): str => prefix + name;\n\
                     greet(\"ada\")\n\
                   }";
        assert_eq!(run_str(src, "f"), "hi ada");
    }

    #[test]
    fn closures_in_a_list() {
        let src = "function adder(by: i64): (i64) => i64 { (n: i64): i64 => n + by }\n\
                   function f(): i64 {\n\
                     var fns: List<(i64) => i64> = [adder(1), adder(2), adder(3)];\n\
                     var total = 0;\n\
                     for g in fns { total = total + g(10); }\n\
                     total\n\
                   }";
        // (10+1) + (10+2) + (10+3) = 36
        assert_eq!(run(src, "f"), 36);
    }

    #[test]
    fn map_index_read_and_write() {
        let src = "function f(): i64 {\n\
                     var m: Map<str, i64> = { \"a\": 1 };\n\
                     m[\"b\"] = 20;\n\
                     m[\"a\"] = 10;\n\
                     m[\"a\"] + m[\"b\"]\n\
                   }";
        assert_eq!(run(src, "f"), 30);
    }

    #[test]
    fn map_index_int_keys() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 100 };\n\
                     m[2] = 50;\n\
                     m[1] + m[2]\n\
                   }";
        assert_eq!(run(src, "f"), 150);
    }

    #[test]
    fn for_entry_in_map_sums_values() {
        let src = "function f(): i64 {\n\
                     var m: Map<i64, i64> = { 1: 10, 2: 20, 3: 12 };\n\
                     var total = 0;\n\
                     for e in m { total = total + e.key + e.value; }\n\
                     total\n\
                   }";
        // keys 1+2+3=6, values 10+20+12=42 => 48
        assert_eq!(run(src, "f"), 48);
    }

    #[test]
    fn for_entry_in_map_str_keys() {
        let src = "function f(): str {\n\
                     var m: Map<str, str> = { \"k\": \"v\" };\n\
                     var out = \"\";\n\
                     for e in m { out = out + e.key + \"=\" + e.value; }\n\
                     out\n\
                   }";
        assert_eq!(run_str(src, "f"), "k=v");
    }

    // -- generic `extend` method resolution ----------------------------------

    const PAIR_SRC: &str = "\
struct Pair<A, B> { first: A, second: B }\n\
extend<A, B> Pair<A, B> {\n\
  function fst(self): A { self.first }\n\
  function snd(self): B { self.second }\n\
  function swap(self): Pair<B, A> { Pair { first: self.second, second: self.first } }\n\
}\n";

    #[test]
    fn generic_extend_method_returns_param() {
        let src = format!(
            "{PAIR_SRC}function f(): i64 {{ var p = Pair {{ first: 42, second: 7 }}; p.fst() }}"
        );
        assert_eq!(run(&src, "f"), 42);
    }

    #[test]
    fn generic_extend_method_second_param() {
        let src = format!(
            "{PAIR_SRC}function f(): str {{\n\
               var p = Pair {{ first: 1, second: \"hi\" }};\n\
               p.snd()\n\
             }}"
        );
        assert_eq!(run_str(&src, "f"), "hi");
    }

    #[test]
    fn generic_extend_method_returning_generic_struct() {
        let src = format!(
            "{PAIR_SRC}function f(): i64 {{\n\
               var p = Pair {{ first: 5, second: 9 }};\n\
               var s = p.swap();\n\
               s.fst() * 10 + s.snd()\n\
             }}"
        );
        // swap => Pair{first:9, second:5}; 9*10+5 = 95
        assert_eq!(run(&src, "f"), 95);
    }

    #[test]
    fn generic_extend_mutating_method() {
        let src = "struct Cell<T> { value: T }\n\
                   extend<T> Cell<T> {\n\
                     function get(self): T { self.value }\n\
                     function put(self, v: T) { self.value = v; }\n\
                   }\n\
                   function f(): i64 { var c = Cell { value: 1 }; c.put(77); c.get() }";
        assert_eq!(run(src, "f"), 77);
    }

    #[test]
    fn for_over_bounded_type_parameter() {
        let src = format!(
            "{RANGE_SRC}\
             function sum<T: Iterator<i64>>(it: T): i64 {{\n\
               var total = 0;\n\
               for x in it {{ total = total + x; }}\n\
               total\n\
             }}\n\
             function f(): i64 {{ sum(Range {{ current: 1, end: 5 }}) }}"
        );
        // 1 + 2 + 3 + 4 = 10
        assert_eq!(run(&src, "f"), 10);
    }

    #[test]
    fn interface_object_satisfies_its_own_bound() {
        // A `dyn Iterator<i64>` value is accepted where `T: Iterator<i64>`.
        let src = format!(
            "{RANGE_SRC}\
             function sum<T: Iterator<i64>>(it: T): i64 {{\n\
               var t = 0;\n\
               for x in it {{ t = t + x; }}\n\
               t\n\
             }}\n\
             function f(): i64 {{\n\
               var it: Iterator<i64> = Range {{ current: 1, end: 4 }};\n\
               sum(it)\n\
             }}"
        );
        // 1 + 2 + 3 = 6
        assert_eq!(run(&src, "f"), 6);
    }

    #[test]
    fn for_over_interface_object_iterator() {
        let src = format!(
            "{RANGE_SRC}\
             function f(): i64 {{\n\
               var it: Iterator<i64> = Range {{ current: 10, end: 13 }};\n\
               var s = 0;\n\
               for y in it {{ s = s + y; }}\n\
               s\n\
             }}"
        );
        // 10 + 11 + 12 = 33
        assert_eq!(run(&src, "f"), 33);
    }

    #[test]
    fn generic_iterator_in_for_loop() {
        let src = "struct Count<T> { value: T, n: i64 }\n\
                   extend<T> Count<T>: Iterator<T> {\n\
                     function next(self): Item<T> | Done {\n\
                       if self.n <= 0 { Done {} }\n\
                       else { self.n = self.n - 1; Item { value: self.value } }\n\
                     }\n\
                   }\n\
                   function f(): i64 {\n\
                     var total = 0;\n\
                     for x in (Count { value: 7, n: 4 }) { total = total + x; }\n\
                     total\n\
                   }";
        // 7 added 4 times = 28
        assert_eq!(run(src, "f"), 28);
    }

    // -- interface objects / dynamic dispatch --------------------------------

    const ANIMALS_SRC: &str = "\
interface Sound { function code(self): i64; }\n\
struct Dog {}\n\
struct Cat {}\n\
extend Dog: Sound { function code(self): i64 { 1 } }\n\
extend Cat: Sound { function code(self): i64 { 2 } }\n";

    #[test]
    fn dyn_dispatch_through_interface_value() {
        let src = format!(
            "{ANIMALS_SRC}\
             function f(): i64 {{\n\
               var a: Sound = Dog {{}};\n\
               var b: Sound = Cat {{}};\n\
               a.code() * 10 + b.code()\n\
             }}"
        );
        // Dog=1, Cat=2 => 12
        assert_eq!(run(&src, "f"), 12);
    }

    #[test]
    fn dyn_dispatch_through_parameter() {
        let src = format!(
            "{ANIMALS_SRC}\
             function tell(s: Sound): i64 {{ s.code() }}\n\
             function f(): i64 {{ tell(Dog {{}}) + tell(Cat {{}}) * 100 }}"
        );
        // 1 + 2*100 = 201
        assert_eq!(run(&src, "f"), 201);
    }

    #[test]
    fn dyn_dispatch_over_heterogeneous_list() {
        let src = format!(
            "{ANIMALS_SRC}\
             function f(): i64 {{\n\
               var zoo: List<Sound> = [Dog {{}}, Cat {{}}, Cat {{}}];\n\
               var total = 0;\n\
               for s in zoo {{ total = total + s.code(); }}\n\
               total\n\
             }}"
        );
        // 1 + 2 + 2 = 5
        assert_eq!(run(&src, "f"), 5);
    }

    #[test]
    fn dyn_method_with_argument_and_str() {
        let src = "interface Greeter { function greet(self, name: str): str; }\n\
                   struct Formal { title: str }\n\
                   extend Formal: Greeter {\n\
                     function greet(self, name: str): str { self.title + name }\n\
                   }\n\
                   function f(): str {\n\
                     var g: Greeter = Formal { title: \"Dr \" };\n\
                     g.greet(\"Ada\")\n\
                   }";
        assert_eq!(run_str(src, "f"), "Dr Ada");
    }

    #[test]
    fn dyn_is_checks_concrete_type() {
        let src = format!(
            "{ANIMALS_SRC}\
             function f(): i64 {{\n\
               var s: Sound = Dog {{}};\n\
               var a = if s is Dog {{ 1 }} else {{ 0 }};\n\
               var b = if s is Cat {{ 1 }} else {{ 0 }};\n\
               a * 10 + b\n\
             }}"
        );
        // is Dog => 1, is Cat => 0 => 10
        assert_eq!(run(&src, "f"), 10);
    }

    #[test]
    fn dyn_as_downcasts_to_concrete() {
        let src = "interface Show { function show(self): i64; }\n\
                   struct Boxed { n: i64 }\n\
                   extend Boxed: Show { function show(self): i64 { self.n } }\n\
                   function f(): i64 {\n\
                     var s: Show = Boxed { n: 42 };\n\
                     var b = s as Boxed;\n\
                     b.n\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn dyn_narrowing_then_field_access() {
        let src = "interface Show { function show(self): i64; }\n\
                   struct A { x: i64 }\n\
                   struct B {}\n\
                   extend A: Show { function show(self): i64 { self.x } }\n\
                   extend B: Show { function show(self): i64 { -1 } }\n\
                   function f(): i64 {\n\
                     var s: Show = A { x: 9 };\n\
                     if s is A { var a = s as A; a.x } else { 0 }\n\
                   }";
        assert_eq!(run(src, "f"), 9);
    }

    #[test]
    fn for_in_list_sum() {
        let src = "function f(): i64 {\n\
                     var total: i64 = 0;\n\
                     for x in [10, 20, 12] { total = total + x; }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn for_in_list_with_break() {
        let src = "function f(): i64 {\n\
                     var total: i64 = 0;\n\
                     for x in [1, 2, 3, 100, 200] {\n\
                       if x > 10 { break }\n\
                       total = total + x;\n\
                     }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 6);
    }

    #[test]
    fn for_in_list_with_continue() {
        let src = "function f(): i64 {\n\
                     var total: i64 = 0;\n\
                     for x in [1, 2, 3, 4, 5, 6] {\n\
                       if x % 2 == 1 { continue }\n\
                       total = total + x;\n\
                     }\n\
                     total\n\
                   }";
        assert_eq!(run(src, "f"), 12);
    }

    #[test]
    fn for_over_list_param() {
        let src = "function sum(xs: List<i64>): i64 {\n\
                     var t: i64 = 0;\n\
                     for x in xs { t = t + x; }\n\
                     t\n\
                   }\n\
                   function f(): i64 { sum([3, 4, 5, 30]) }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- generics (monomorphization) ---------------------------------------

    #[test]
    fn generic_identity_inferred() {
        let src = "function id<T>(x: T): T { x }\n\
                   function f(): i64 { id(42) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_identity_two_instances() {
        // Used at i64 and str — two monomorphizations from one body.
        let src = "function id<T>(x: T): T { x }\n\
                   function f(): i64 { id(40) + 2 }\n\
                   function s(): str { id(\"hi\") }";
        assert_eq!(run(src, "f"), 42);
        assert_eq!(run_str(src, "s"), "hi");
    }

    #[test]
    fn generic_explicit_type_args() {
        let src = "function id<T>(x: T): T { x }\n\
                   function f(): i64 { id<i64>(42) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_two_params() {
        let src = "function first<A, B>(a: A, b: B): A { a }\n\
                   function f(): i64 { first(42, \"ignored\") }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_transitive_instantiation() {
        // `wrap` calls `id` with its own `T`; instantiating wrap<i64> must
        // transitively instantiate id<i64>.
        let src = "function id<T>(x: T): T { x }\n\
                   function wrap<T>(x: T): T { id(x) }\n\
                   function f(): i64 { wrap(42) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_struct_box() {
        let src = "struct Box<T> { value: T }\n\
                   function f(): i64 { var b = Box<i64> { value: 42 }; b.value }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn generic_max_with_bound() {
        let src = "function pick<T>(a: T, b: T, useA: bool): T { if useA { a } else { b } }\n\
                   function f(): i64 { pick(42, 7, true) }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- `?` propagation ---------------------------------------------------

    const TRY_SRC: &str = "\
        function parse(ok: bool): i64 | str { if ok { 42 } else { \"bad\" } }\n\
        function f(ok: bool): str {\n\
          var n: i64 = parse(ok)?;\n\
          \"n=\" + (n as str)\n\
        }\n";

    #[test]
    fn try_success_path_unwraps() {
        let src = format!("{TRY_SRC}function g(): str {{ f(true) }}");
        assert_eq!(run_str(&src, "g"), "n=42");
    }

    #[test]
    fn try_error_path_propagates() {
        // The `str` error variant is returned early as `f`'s `str` result.
        let src = format!("{TRY_SRC}function g(): str {{ f(false) }}");
        assert_eq!(run_str(&src, "g"), "bad");
    }

    #[test]
    fn try_propagates_into_union_return() {
        let src = "function parse(ok: bool): i64 | str { if ok { 7 } else { \"e\" } }\n\
                   function f(ok: bool): bool | str { var n: i64 = parse(ok)?; n > 3 }\n\
                   function g(): i64 { var r = f(true); if r is bool { if r as bool { 42 } else { 0 } } else { 1 } }";
        assert_eq!(run(&src, "g"), 42);
    }

    // --- flow narrowing ----------------------------------------------------

    #[test]
    fn narrowing_uses_variant_without_explicit_as() {
        // In the `is i64` branch, `x` is usable as `i64` directly.
        let src = "function f(x: i64 | str): i64 { if x is i64 { x + 1 } else { 0 } }\n\
                   function g(): i64 { f(41) }";
        assert_eq!(run(src, "g"), 42);
    }

    #[test]
    fn narrowing_else_branch_complement() {
        // After `if x is null { .. }`, the else branch narrows to `i64`.
        let src = "function f(x: i64 | null): i64 { if x is null { 0 } else { x + 2 } }\n\
                   function g(): i64 { f(40) }";
        assert_eq!(run(src, "g"), 42);
    }

    #[test]
    fn narrowing_struct_field_access() {
        let src = "struct P { v: i64 }\n\
                   function f(x: P | null): i64 { if x is P { x.v } else { 0 } }\n\
                   function g(): i64 { f(P { v: 42 }) }";
        assert_eq!(run(src, "g"), 42);
    }

    #[test]
    fn narrowing_else_if_chain_composes() {
        // The final else narrows to the single remaining variant (str).
        let src = "function d(x: i64 | str | null): str {\n\
                     if x is null { \"none\" }\n\
                     else if x is i64 { \"int $x\" }\n\
                     else { \"s:$x\" }\n\
                   }\n\
                   function g(): str { d(\"hi\") }";
        assert_eq!(run_str(src, "g"), "s:hi");
    }

    #[test]
    fn narrowing_str_branch_interpolates() {
        let src = "function f(x: i64 | str): str { if x is str { \"got $x\" } else { \"num\" } }\n\
                   function g(): str { f(\"hi\") }";
        assert_eq!(run_str(src, "g"), "got hi");
    }

    // --- string interpolation ----------------------------------------------

    #[test]
    fn interpolate_ident() {
        let src = "function f(): str { var name = \"Alice\"; \"hi $name\" }";
        assert_eq!(run_str(src, "f"), "hi Alice");
    }

    #[test]
    fn interpolate_expr_and_number() {
        let src = "function f(): str { var n: i64 = 40; \"total: ${n + 2}\" }";
        assert_eq!(run_str(src, "f"), "total: 42");
    }

    #[test]
    fn interpolate_multiple_parts() {
        let src = "function f(): str {\n\
                     var x: i64 = 3;\n\
                     var y: i64 = 4;\n\
                     \"$x + $y = ${x + y}\"\n\
                   }";
        assert_eq!(run_str(src, "f"), "3 + 4 = 7");
    }

    #[test]
    fn interpolate_bool_and_char() {
        let src = "function f(): str { var b: bool = true; var c: char = 'Z'; \"$b $c\" }";
        assert_eq!(run_str(src, "f"), "true Z");
    }

    #[test]
    fn interpolate_escaped_dollar() {
        let src = "function f(): str { \"price: \\$5\" }";
        assert_eq!(run_str(src, "f"), "price: $5");
    }

    // --- match -------------------------------------------------------------

    #[test]
    fn match_union_variants() {
        let src = "function describe(x: i64 | str): i64 {\n\
                     match x { i64 n => n, str s => 0 }\n\
                   }\n\
                   function f(): i64 { describe(42) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_union_picks_right_arm() {
        let src = "function describe(x: i64 | str): i64 {\n\
                     match x { i64 n => n + 100, str s => 42 }\n\
                   }\n\
                   function f(): i64 { describe(\"hi\") }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_literals_with_wildcard() {
        let src = "function f(): i64 { var n: i64 = 2; match n { 1 => 10, 2 => 42, _ => 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_null_union() {
        let src = "function f(): i64 { var x: i64 | null = 42; match x { i64 n => n, null => 0 } }";
        assert_eq!(run(src, "f"), 42);
        let src2 = "function f(): i64 { var x: i64 | null = null; match x { i64 n => n, null => 42 } }";
        assert_eq!(run(src2, "f"), 42);
    }

    #[test]
    fn match_with_guard() {
        let src = "function f(): i64 { var n: i64 = 5; match n { i64 x if x > 3 => 42, _ => 0 } }";
        assert_eq!(run(src, "f"), 42);
        let src2 = "function f(): i64 { var n: i64 = 1; match n { i64 x if x > 3 => 0, _ => 42 } }";
        assert_eq!(run(src2, "f"), 42);
    }

    #[test]
    fn match_unit_struct_variants() {
        let src = "struct Red;\nstruct Green;\nstruct Blue;\n\
                   type Color = Red | Green | Blue;\n\
                   function code(c: Color): i64 { match c { Red => 1, Green => 2, Blue => 42 } }\n\
                   function f(): i64 { code(Blue) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_tuple_destructure() {
        let src = "function f(): i64 { var t = (40, 2); match t { (a, b) => a + b } }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn match_extracts_struct_payload() {
        let src = "struct P { v: i64 }\n\
                   function f(): i64 { var x: P | null = P { v: 42 }; match x { P p => p.v, null => 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- str methods & comparison ------------------------------------------

    #[test]
    fn str_size_and_byte_size() {
        // "héllo": 5 scalars, 6 UTF-8 bytes.
        assert_eq!(run("function f(): i64 { \"héllo\".size() }", "f"), 5);
        assert_eq!(run("function f(): i64 { \"héllo\".byte_size() }", "f"), 6);
    }

    #[test]
    fn str_substring_and_case() {
        assert_eq!(run_str("function f(): str { \"hello world\".substring(0, 5) }", "f"), "hello");
        assert_eq!(run_str("function f(): str { \"abc\".to_upper() }", "f"), "ABC");
        assert_eq!(run_str("function f(): str { \"  hi  \".trim() }", "f"), "hi");
    }

    #[test]
    fn str_predicates() {
        assert_eq!(run("function f(): bool { \"hello\".contains(\"ell\") }", "f"), 1);
        assert_eq!(run("function f(): bool { \"hello\".starts_with(\"he\") }", "f"), 1);
        assert_eq!(run("function f(): bool { \"hello\".ends_with(\"lo\") }", "f"), 1);
        assert_eq!(run("function f(): bool { \"\".is_empty() }", "f"), 1);
    }

    #[test]
    fn str_equality_by_content() {
        // Two distinct str objects with equal content must compare equal.
        let src = "function f(): i64 { var a = \"x\" + \"y\"; var b = \"xy\"; if a == b { 42 } else { 0 } }";
        assert_eq!(run(src, "f"), 42);
        let ne = "function f(): i64 { if \"a\" != \"b\" { 42 } else { 0 } }";
        assert_eq!(run(ne, "f"), 42);
    }

    #[test]
    fn str_ordering_lexicographic() {
        assert_eq!(run("function f(): bool { \"apple\" < \"banana\" }", "f"), 1);
        assert_eq!(run("function f(): bool { \"banana\" < \"apple\" }", "f"), 0);
        assert_eq!(run("function f(): bool { \"abc\" <= \"abc\" }", "f"), 1);
    }

    // --- discriminated unions ----------------------------------------------

    #[test]
    fn union_widen_then_narrow() {
        let src = "function f(): i64 { var x: i64 | str = 40; (x as i64) + 2 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn union_is_test() {
        let yes = "function f(): i64 { var x: i64 | str = \"hi\"; if x is str { 42 } else { 0 } }";
        assert_eq!(run(yes, "f"), 42);
        let no = "function f(): i64 { var x: i64 | str = \"hi\"; if x is i64 { 0 } else { 42 } }";
        assert_eq!(run(no, "f"), 42);
    }

    #[test]
    fn optional_null_union() {
        let null_case =
            "function f(): i64 { var x: i64 | null = null; if x is null { 42 } else { 0 } }";
        assert_eq!(run(null_case, "f"), 42);
        let some_case =
            "function f(): i64 { var x: i64 | null = 40; if x is i64 { (x as i64) + 2 } else { 0 } }";
        assert_eq!(run(some_case, "f"), 42);
    }

    #[test]
    fn union_returned_from_function() {
        let src = "function pick(b: bool): i64 | null { if b { 42 } else { null } }\n\
                   function f(): i64 { var r = pick(true); if r is i64 { r as i64 } else { 0 } }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn struct_in_union() {
        let src = "struct P { v: i64 }\n\
                   function f(): i64 { var x: P | null = P { v: 42 }; (x as P).v }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn widen_union_to_wider_union() {
        let src = "function f(): i64 {\n\
                     var a: i64 | str = 40;\n\
                     var b: i64 | str | bool = a;\n\
                     (b as i64) + 2\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn union_str_variant_roundtrip() {
        let src = "function f(): str { var x: i64 | str = \"hello\"; x as str }";
        assert_eq!(run_str(src, "f"), "hello");
    }

    // --- anonymous tuples --------------------------------------------------

    #[test]
    fn tuple_construct_and_index() {
        let src = "function f(): i64 { var t = (40, 2); t.0 + t.1 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_destructure() {
        let src = "function f(): i64 { var (a, b) = (40, 2); a + b }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_nested_destructure() {
        let src = "function f(): i64 { var (a, (b, c)) = (40, (1, 1)); a + b + c }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_mixed_types() {
        // (i64, str, bool) — read the i64 and use the str via run.
        let src = "function f(): str { var t = (42, \"hi\", true); t.1 }";
        assert_eq!(run_str(src, "f"), "hi");
    }

    #[test]
    fn tuple_returned_from_function() {
        let src = "function pair(): (i64, i64) { (40, 2) }\n\
                   function f(): i64 { var t = pair(); t.0 + t.1 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn tuple_index_wildcard_destructure() {
        let src = "function f(): i64 { var (a, _, c) = (40, 99, 2); a + c }";
        assert_eq!(run(src, "f"), 42);
    }

    // --- methods (extend) --------------------------------------------------

    #[test]
    fn inherent_method_reads_self() {
        let src = "struct P { x: i64, y: i64 }\n\
                   extend P { function sum(self): i64 { self.x + self.y } }\n\
                   function f(): i64 { var p = P { x: 40, y: 2 }; p.sum() }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn method_with_arguments() {
        let src = "struct Counter { n: i64 }\n\
                   extend Counter { function add(self, k: i64): i64 { self.n + k } }\n\
                   function f(): i64 { var c = Counter { n: 40 }; c.add(2) }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn method_mutates_self() {
        let src = "struct Counter { n: i64 }\n\
                   extend Counter { function bump(self) { self.n = self.n + 1; } }\n\
                   function f(): i64 { var c = Counter { n: 41 }; c.bump(); c.n }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn method_calls_method() {
        let src = "struct P { x: i64 }\n\
                   extend P {\n\
                     function doubled(self): i64 { self.x + self.x }\n\
                     function quad(self): i64 { self.doubled() + self.doubled() }\n\
                   }\n\
                   function f(): i64 { var p = P { x: 10 }; p.quad() + 2 }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn methods_on_different_types_same_name() {
        // Two types each define `val`; mangled symbols must not collide.
        let src = "struct A { a: i64 }\n\
                   struct B { b: i64 }\n\
                   extend A { function val(self): i64 { self.a } }\n\
                   extend B { function val(self): i64 { self.b * 2 } }\n\
                   function f(): i64 {\n\
                     var x = A { a: 40 };\n\
                     var y = B { b: 1 };\n\
                     x.val() + y.val()\n\
                   }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn method_returning_self_type() {
        let src = "struct V { x: i64 }\n\
                   extend V { function plus(self, o: V): V { V { x: self.x + o.x } } }\n\
                   function f(): i64 { var a = V { x: 40 }; var b = V { x: 2 }; a.plus(b).x }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn struct_aliasing_shares_heap() {
        // b = a aliases the same object; mutating through b is visible via a.
        let src = "struct P { x: i64 }\n\
                   function f(): i64 { var a = P { x: 1 }; var b = a; b.x = 42; a.x }";
        assert_eq!(run(src, "f"), 42);
    }

    #[test]
    fn nested_loops_break_innermost() {
        let src = "function f(): i64 {\n\
                     var count: i64 = 0;\n\
                     var i: i64 = 0;\n\
                     while i < 3 {\n\
                       var j: i64 = 0;\n\
                       while j < 100 {\n\
                         if j >= 2 { break }\n\
                         count = count + 1;\n\
                         j = j + 1;\n\
                       }\n\
                       i = i + 1;\n\
                     }\n\
                     count\n\
                   }";
        // inner loop adds 2 each of 3 outer iterations = 6
        assert_eq!(run(src, "f"), 6);
    }
