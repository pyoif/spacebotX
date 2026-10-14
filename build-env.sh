#!/bin/bash
# Build environment for spacebotX on this host.
#
# The sandbox ships no system C toolchain, no OpenSSL dev headers, no
# pkg-config, and no protoc. We borrow all of them from the persistent pixi
# envs under the workspace, plus the protoc binary and well-known .proto
# includes staged under ~/.ccbin and ~/protoc-dl by an earlier build attempt.
set -a
export CARGO_HOME="$HOME/.cargo"
export RUSTUP_HOME="$HOME/.rustup"

# .ccbin has cc/c++/ld/ar/ranlib/strip/nm/objcopy symlinks + pkg-config + protoc.
export PATH="$HOME/.ccbin:$HOME/.cargo/bin:$PATH"

export CC="$HOME/.ccbin/cc"
export CXX="$HOME/.ccbin/c++"
export AR="$HOME/.ccbin/ar"
export LD="$HOME/.ccbin/ld"
export RANLIB="$HOME/.ccbin/ranlib"

# OpenSSL dev (from the pixi openssl env, symlinked into .ccbin's pkg path).
export OPENSSL_DIR="$HOME/.pixi/envs/openssl"
export PKG_CONFIG_PATH="$HOME/.pixi/envs/openssl/lib/pkgconfig"

# protoc + well-known protobuf includes.
export PROTOC="$HOME/.ccbin/protoc"

export LIBCLANG_PATH=""
set +a
