# setl8-vault

Build with `scripts/anchor-build-checked.sh` instead of plain `anchor build`: `anchor build` exits 0 even when the SBF toolchain reports a stack-frame overflow (a function frame over 4,096 bytes), and such a program can silently corrupt memory at runtime. The script fails on that warning and prints the `.so` sha256.
