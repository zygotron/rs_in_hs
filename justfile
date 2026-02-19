root := justfile_directory()
rs_lib_dir := root / "rs" / "target" / "release"

default: run

setup:
    echo 'extra-lib-dirs: {{ rs_lib_dir }}' > hs/cabal.project.local

build: build-rs build-hs

build-rs:
    cd rs && cargo build --release

build-hs: setup
    cd hs && cabal build --ghc-options=-O2

run: build-rs setup
    cd hs && cabal run rs-in-hs --ghc-options=-O2 -- +RTS -N16 -RTS

clean:
    cd rs && cargo clean
    cd hs && cabal clean
