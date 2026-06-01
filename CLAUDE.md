We are designing and building a language called "Otter Fusion". Extension format `.otter`. 

We prefer most of the compiler code to be name agnostic but do use the name in places where it makes sense (like the extension, or the CLI name).

This language is aimed to be at the same level or rust, c, go or any other production ready language. Anything less is insulting. 

**We do not take shortcuts** 

**We are not building a prototype or proof of concept**

**We are building a well architected fully featured end to end working language**

We keep track goals in goals.txt. You can read it for more context. Goals that are definitely done are put before "---" and pending ones after. We automatically update goals.txt when we complete a goal. We also add new goals when planning new features or splitting existing ones.

# RULES

- Do not take shortcuts
- Follow the correct architecture decisions
- Follow docs, you cannot change language design without asking (but if it make sense to change something please ask)
- Make code modular clean and organized, use multiple modules/crates, document functions
- ALWAYS do unit and integration tests, cover as many cases as possible
- Do integration tests in a large variety and quantity. 
- Make end to end (or close to it) tests
- Keep dependencies and logic updated
- Implement the compiler in a efficient way and ensure the generated code is efficient as well.
- Ensure examples are working and up to date
- Ensure docs are up to date and consistent
- Keep roadmap and your memory updated and consistent with the actual state of the project
- Read docs whenever is needed
- Approach building the lenguage as if you were building a production ready language, with the same quality bar as rust, c, go or any other production ready language. Anything less is insulting.
- Follow design inspiration from how rust does stuff. Rust is our base for features, design and architecture.
- Follow docs and keep them updated, don't deviate unless is necessary, in this case ask me first. You are allowed to make design decisions but ask me first if it is a big thing that will change the language itself
- Keep test suite up to date with many many test cases. We test all, not oly happy paths but compilation error, panics, memory things, things that should not compile or work but they do!
- See the bigger pictures, features are not isolated. Ensure implementations are consistent with the rest of the language and ecosystem, and that they work well together.
- DO NOT USE FEATURE BRANCHES UNLESS APPROVED EXPLICITLY. THERE IS NO SUCH THING AS IMPLICIT APPROVE. 