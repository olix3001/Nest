# What is Nest?
Nest is a modern programming language designed for readability, simplicity, and ease of use.
Even though Nest is a compiled programming language, it has a garbage collector, which at the moment is Boehm GC, but the compiler already produces metadata that will allow for easy replacement with precise GC in near future.

# Main Language Goals
- Syntax should stay readable, yet simple, rather than easy to parse. That is why every definition and declaration uses `::` syntax, as this does not introduce new, unnecessary complexity.
- Type system should be powerful. When programming, you should not have to write what's already known, that is why Nest has very strong type inference and type system.
- Simplicity is the key, and that is exactly why Nest aims to provide syntax sugar for many features that would otherwise require writing a few lines of code.
- More features is always better, as long as they do not fight each other. If the language has a feature that some user does not want, they can just act as if it's not there, but the other way is a problem. That is why I believe more language features is always better!

# Platform Support
This language uses LLVM, which fortunately makes most targets supported by default. In theory, every target that LLVM supports, and that has libc and Boehm GC support should work. In the future, when no-std support is introduced and custom GC is fully implemented, all LLVM targets should be supported.

The official (tested) support stays limited to the following targets:
`x86_64 Linux`, `arm64 MacOS`. (Windows should work too, but first I need to find good prebuilt LLVM for it or CI runs for )

# Getting Started
There are no docs nor any installer at the moment, so this section will be written in the future.

# Compiling From Source
Compiling from source requires the following dependencies to be available on your system:
- LLVM 21 with `LLVM_SYS_211_PREFIX` environment variable set,
- Boehm GC with `BDW_GC_PREFIX` environment variable set (temporary),
- Rust nightly with cargo available on your system (deref_patterns feature is used),
- C++ compiler available under `cc` command,
- `justfile` command runner ([Github](https://github.com/casey/just))

You can check whether everything is setup correctly by running `just tools` command.
To build everything, you can use `just build` command, or specify a profile with `just profile=debug build`.
You can also run all tests using `just test` or `just profile=debug test`.

After building everything, there will be a message with all target binaries listed. You can add their directories to the PATH or move into some bin diretory.

# About Stability
This language should be considered highly experimental, no syntax, IR/LIR format, language feature, manifest format or CLI tool should be thought of as stable. There is a possibility that **everything** changes in the future. The only thing I can guarantee is that `twig build` and `twig run` commands will not change, but their arguments and output can.

# About AI Usage
I do not want to pretend as if this project was made without any AI usage, as it clearly was. The goal of the current compiler is to play around and test what works and what does not. There is a plan to rewrite this compiler in the future, so that the language can become self-hosted. That is why at the moment the language is highly experimental and the syntax should be considered unstable.

However, despite AI writing large part of this code, I will not accept any slop in the pull requests. The code should be checked manually and commits should not have a few thousand lines of changes with long descriptions and changes to features not even related to the PR.
