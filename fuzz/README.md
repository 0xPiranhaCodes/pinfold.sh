# Fuzz targets

`cargo-fuzz` targets go here, starting with the VT input path in M1. This folder stays
outside the workspace because fuzzing needs a nightly toolchain. CI skips fuzz smoke
until the first target exists.
