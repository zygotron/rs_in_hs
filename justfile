root := justfile_directory()
rs_lib_dir := root / "rs" / "target" / "release"

default: run

build: build-rs build-hs

build-rs:
    cd rs && cargo build --release

build-hs:
    cd hs && cabal build --extra-lib-dirs={{ rs_lib_dir }} --ghc-options=-O2

run: build-rs
    cd hs && cabal run rs-in-hs --extra-lib-dirs={{ rs_lib_dir }} --ghc-options=-O2 -- +RTS -N16 -RTS

clean:
    cd rs && cargo clean
    cd hs && cabal clean
