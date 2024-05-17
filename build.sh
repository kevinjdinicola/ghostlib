HEADERPATH="bindings/ghostlibFFI.h"
TARGETDIR="target"
OUTDIR="framework_out"
RELDIR="release"
NAME="ghostlib"
STATIC_LIB_NAME="lib${NAME}.a"
NEW_HEADER_DIR="bindings/include"

echo "Building"
#cargo build --target aarch64-apple-darwin --release
cargo build --target aarch64-apple-ios --release
cargo build --target aarch64-apple-ios-sim --release

echo "Generating Bindings"
cargo run --bin uniffi-bindgen generate --library target/aarch64-apple-ios-sim/release/libghostlib.a --language swift --out-dir bindings

echo "Bundling framework"
mkdir -p "${NEW_HEADER_DIR}"
cp "${HEADERPATH}" "${NEW_HEADER_DIR}/"
cp "bindings/ghostlibFFI.modulemap" "${NEW_HEADER_DIR}/module.modulemap"

rm -rf "${OUTDIR}/${NAME}_framework.xcframework"

#xcodebuild -create-xcframework \
#  -library target/aarch64-apple-ios-sim/release/libghostlib.a \
#  -headers bindings/include \
#  -output framework_out/ghostlib_framework.xcframework

#   -library "${TARGETDIR}/${STATIC_LIB_NAME}" \
#    -headers "${NEW_HEADER_DIR}" \


xcodebuild -create-xcframework \
    -library "${TARGETDIR}/aarch64-apple-ios/${RELDIR}/${STATIC_LIB_NAME}" \
    -headers "${NEW_HEADER_DIR}" \
    -library "${TARGETDIR}/aarch64-apple-ios-sim/${RELDIR}/${STATIC_LIB_NAME}" \
    -headers "${NEW_HEADER_DIR}" \
    -output "${OUTDIR}/${NAME}_framework.xcframework"