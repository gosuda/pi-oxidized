# REL-R1 — musl toolchain & userland bakeoff (winning recipe + evidence)

Stable ID `REL-R1`, issue metaphorics/pi-oxidized#102. This document records
the **winning** per-arch musl C toolchain and userland provisioning recipe,
the executed evidence for x86_64, the source-derived aarch64 legs pending
REL-T3 CI execution, sha256 pins and acquisition dates for every downloaded
artifact, and the deleted-loser note.

Authored 2026-08-26; x86_64 evidence executed 2026-08-27 on a local
Ubuntu 26.04 x86_64 host (no-root, relocated musl prefix). aarch64 legs are
source-derived from primary sources (musl.cc manifest probe, Alpine aarch64
APKINDEX, Launchpad noble arm64 package publication, ziglang.org
`download/index.json`, crates.io, Bun `SHASUMS256.txt`) with pin verification
by download on the x86_64 host; build and smoke execution is pending REL-T3
CI on `ubuntu-24.04-arm`.

---

## 1. Scope

Issue #102 asks which per-arch musl C toolchain and musl userland recipe
actually:

1. build static `pi` for the matching musl triple (including `aws-lc-sys`,
   `zstd-sys`, and `ring`);
2. execute the compiled Bun musl sidecar through the JSONL `hello` protocol;
3. execute the pinned Bun musl runtime + bundled host JS through the same
   protocol;
4. record sha256 pins and acquisition dates for every explicit direct
   download managed by this recipe.

This commit surface is intentionally docs + temporary workflow only. It does
not modify `scripts/release/**`, `packages/**`, `crates/**`, `deny.toml`, or
`release-verification.yml`. Those belong to REL-T1 / REL-T2 / REL-T3.

### Executed vs source-derived summary

| Leg | Status | Evidence |
|---|---|---|
| x86_64 Candidate A — build | **EXECUTED** | `cargo build` exit 0, 19m44s; aws-lc-sys + zstd-sys compiled clean under musl-gcc |
| x86_64 — static gate | **EXECUTED** | zero readelf NEEDED; `ldd` → "statically linked" (Ubuntu 26.04 form of "not a dynamic executable") |
| x86_64 — sidecar PT_INTERP | **EXECUTED** | `readelf -lW` → exactly `/lib/ld-musl-x86_64.so.1` |
| x86_64 — runtime PT_INTERP | **EXECUTED** | `readelf -lW` → exactly `/lib/ld-musl-x86_64.so.1` |
| x86_64 — loader isolation | **EXECUTED** | `ld-musl --list` resolves only to `$USERLAND/usr/lib` + pinned loader; default-path control fails |
| x86_64 — compiled hello smoke | **EXECUTED** | ack JSON fields match exactly |
| x86_64 — runtime hello smoke | **EXECUTED** | ack JSON fields match exactly |
| x86_64 — pi --version | **EXECUTED** | workspace version (see [compatibility.md](compatibility.md)) |
| x86_64 — ledger gate | **EXECUTED** | 6/6 expected artifacts, row count match |
| aarch64 Candidate A — build | **SOURCE-DERIVED** | Launchpad confirms noble arm64 `musl-dev`/`musl-tools` 1.2.4-2 Published; same recipe as x86_64; pending REL-T3 CI |
| aarch64 — all gates | **SOURCE-DERIVED** | pins verified by download on x86_64; build+smoke pending REL-T3 CI on `ubuntu-24.04-arm` |
| aarch64 Candidate B — cargo-zigbuild | **SOURCE-DERIVED** | Zig 0.16.0 + cargo-zigbuild 0.23.0 pins re-verified live; conditional backup only; pending REL-T3 CI |
| Candidate C — musl.cc cross tarball | **DELETED** (loser) | see §4 |

---

## 2. Winner-selection rule

Winner **per architecture** = the first candidate that, **natively** on that
architecture's runner, passes every gate below:

1. `cargo build -p pi --release --locked --target <musl-triple>` exits 0
2. `readelf -d` on `pi` shows **zero** `NEEDED` entries
3. `ldd` on `pi` prints `not a dynamic executable` (or `statically linked` on
   systems where ldd uses that form for static-pie binaries)
4. compiled Bun musl sidecar builds for the exact `--target`
5. Alpine-derived musl loader + isolated `$RUNNER_TEMP` libstdc++/libgcc
   userland resolves the sidecar / runtime `NEEDED` set with **no** host
   `/lib`/`/usr/lib` fallback; every `ld-musl --list` object outside
   `$USERLAND_DIR/` is rejected except the exact pinned `/lib/$MUSL_LOADER`
6. staged and unpacked `pi-extension-host` **and** `bun` each have exactly one
   `Requesting program interpreter:` field, and that exact value equals
   `/lib/$MUSL_LOADER` (fail-closed; not a substring; `--list` alone is not
   sufficient proof of the embedded interpreter)
7. unpacked `pi --version` succeeds
8. unpacked compiled-sidecar `hello` ack validates exact JSON fields
9. unpacked runtime-bundle `hello` ack validates exact JSON fields
10. the direct-download ledger is complete for this recipe: exact expected
    artifact names and row count for the selected candidate are asserted
    before PASS (x86_64 Candidate A: musl-dev deb + musl-tools deb + minirootfs
    + libstdc++ + libgcc + Bun runtime asset = 6; aarch64 Candidate A: same
    class of four artifacts for aarch64 = 4; aarch64 Candidate B adds Zig +
    cargo-zigbuild crate = 6)

Rules:

- **Winner on x86_64: Candidate A** (executed and passed all gates locally).
- **Expected winner on aarch64: Candidate A** (source-derived; pending REL-T3
  CI execution on `ubuntu-24.04-arm`).
- Candidate B is tried **only on aarch64**, and **only if Candidate A build
  fails**. Candidate A evidence is retained regardless.
- **Candidate C (musl.cc) is deleted from the recipe** — see §4.
- QEMU / `docker/setup-qemu-action` is **not present** and must never count as
  native evidence.
- Before REL-T3, the losing backup command block (Candidate C) has been
  deleted from this recipe; its rejection record remains in §4.

---

## 3. Winning candidate

**Candidate A — apt `musl-tools` + `musl-dev` (Ubuntu noble `1.2.4-2`)**

| Arch | Runner | Rust target | Linker / CC | Bun compile target | Bun runtime asset |
|---|---|---|---|---|---|
| x86_64 | `ubuntu-24.04` | `x86_64-unknown-linux-musl` | `musl-gcc` / `musl-gcc` | `bun-linux-x64-musl-baseline` | `bun-linux-x64-musl-baseline.zip` |
| aarch64 | `ubuntu-24.04-arm` | `aarch64-unknown-linux-musl` | `musl-gcc` / `musl-gcc` | `bun-linux-arm64-musl` | `bun-linux-aarch64-musl.zip` |

Exact Candidate A build commands (also recorded under each evidence tree as
`candidate-A/command.txt`):

```bash
# x86_64
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build -p pi --release --locked --target x86_64-unknown-linux-musl

# aarch64
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
CC_aarch64_unknown_linux_musl=musl-gcc \
  cargo build -p pi --release --locked --target aarch64-unknown-linux-musl
```

### Local no-root x86_64 execution note

The x86_64 evidence was executed on a local Ubuntu 26.04 host without root
access. The exact noble `musl-dev`/`musl-tools` 1.2.4-2 debs were downloaded
from the Ubuntu archive (sha256-verified against the Packages index), extracted
into a local prefix, and a relocated `musl-gcc` wrapper was built with
path-rewritten specs. The Alpine loader was invoked directly via
`--library-path` instead of installing to `/lib` and writing
`/etc/ld-musl-x86_64.path`. In musl's `dynlink.c`, both the path file and the
explicit `--library-path` override replace the default search path entirely,
so the isolation property (no host `/lib`/`/usr/lib` fallback) is identical.

REL-T3 CI on `ubuntu-24.04` will use the root form: `sudo apt-get install
musl-tools musl-dev`, install the Alpine loader at `/lib/ld-musl-x86_64.so.1`,
and write `/etc/ld-musl-x86_64.path` containing exactly `$USERLAND/usr/lib`.

### Userland recipe (both arches, with arch-specific names)

1. Download and digest-check Alpine `minirootfs-3.24.1`.
2. Install the **global loader only** from that rootfs at
   `/lib/ld-musl-<arch>.so.1` (exact pinned path). Do not use host musl.
3. Export a runner-writable absolute userland path inside the acceptance step:
   `export USERLAND_DIR="$RUNNER_TEMP/pi-musl-userland/$ALPINE_ARCH"`.
   GitHub Actions `env:` cannot interpolate `RUNNER_TEMP` reliably, so this
   path is not job-env pinned.
4. Download the exact committed `libstdc++` / `libgcc` `15.2.0-r5` APKs for
   the arch. **Do not** use `APKINDEX` or `apk verify`: a live probe showed
   Alpine 3.24.1 minirootfs keys reject the current v3.24 index/packages as
   `UNTRUSTED signature`. Integrity is the committed content-addressed SHA256
   pin checked before extraction, plus the acquisition UTC date in the ledger.
5. After the SHA256 check, extract `.PKGINFO` and assert exact `pkgname`,
   `pkgver`, and `arch` against the requested values **before** extracting
   `usr/lib` into `$USERLAND_DIR`. **Note:** Alpine `.PKGINFO` uses
   `key = value` syntax (with spaces), not `key=value`. The verification must
   use `grep -Fx "pkgname = $package"` not `grep -Fx "pkgname=$package"`.
   Ledger text must say content-addressed SHA pin + acquisition date; do
   **not** claim signature verification or a vendor-published APK digest.
6. Write `/etc/ld-musl-<arch>.path` with **only** `$USERLAND_DIR/usr/lib`.
   Because an existing path file replaces musl's built-in defaults, omit
   `/lib:/usr/local/lib:/usr/lib` entirely — no host-library fallback.
7. For staged **and** unpacked `pi-extension-host` and `bun`, parse the exact
   `Requesting program interpreter:` field from `readelf -lW` and require the
   extracted path to equal `/lib/$MUSL_LOADER`. Zero or multiple interpreter
   records fail. Explicit `/lib/$MUSL_LOADER --list` is not a substitute for
   this embedded-interpreter check.
8. After every `ld-musl --list` (staged and unpacked host/runtime),
   mechanically reject any resolved object outside `$USERLAND_DIR/` except
   the loader/libc mapping to the exact pinned `/lib/$MUSL_LOADER`. Smoke the
   unpacked binaries only after both the exact `PT_INTERP` assertion and the
   strict `--list` isolation checks pass.

### Host build and hello protocol

Host argv is source-authoritative from `scripts/release/host.ts`
(`hostBundleCommands` / `helloRequestLine` / `isHelloAck`):

```bash
bun build ./src/main.ts --compile --minify \
  --compile-autoload-tsconfig --compile-autoload-package-json \
  --target <bun-compile-target> --outfile "$STAGE/pi-extension-host"

bun build ./src/main.ts --target bun --minify \
  --outfile "$STAGE/pi-extension-host.js"
```

Hello request line:

```json
{"id":1,"kind":"req","method":"hello","payload":{"protocolVersion":1,"compatibilityVersion":"0.80.10"}}
```

Ack fields that must match exactly (not a substring check):

- `kind == "res"`
- `method == "hello"`
- `id == 1`
- `payload.protocolVersion == 1`
- `payload.compatibilityVersion` matches the pinned value (see [compatibility.md](compatibility.md))

All three smoke commands (`pi --version`, compiled hello, runtime hello) run
only against a freshly unpacked archive, with cwd inside the unpack directory,
and only after unpacked exact `PT_INTERP` equality to `/lib/$MUSL_LOADER` and
`ld-musl --list` isolation proofs both pass under the strict path file.

---

## 4. Rejection / deleted-loser reasons

### Candidate B — `cargo-zigbuild` on native `ubuntu-24.04-arm`

- Status: **conditional backup only** (aarch64).
- Trigger: Candidate A provisioning or cargo build fails on aarch64.
- Pins resolved and re-verified live 2026-08-27:
  - Zig `0.16.0` aarch64-linux tarball sha256
    `ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17`
    (ziglang.org `download/index.json` shasum confirmed)
  - `cargo-zigbuild` `0.23.0` crate sha256
    `68c7df45b9d9934aaed5987fbf422b31419f81827b13a52251a61e1e772c6ff7`
    (crates.io API + static.crates.io download confirmed; not yanked)
- Must unset `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER` and
  `CC_aarch64_unknown_linux_musl`; a `-C linker=` value opts cargo-zigbuild out
  of Zig.
- Rejection reason if unused: Candidate A already satisfied the native gates.
- Contingency reason if used: apt musl / musl-gcc path failed on aarch64 while
  a self-contained Zig musl toolchain remains available natively.

### Candidate C — musl.cc `aarch64-linux-musl-cross.tgz` — DELETED

- Status: **deleted from the recipe** (loser).
- Rejection reasons (confirmed by primary-source probe 2026-08-27):
  - **No vendor checksums or signatures**: `https://musl.cc/sha256sum.txt`
    and `https://musl.cc/SHA256SUMS` both return HTTP 404. There is no
    published digest to verify against.
  - **Stale artifact**: the tarball `Last-Modified` header is
    `Tue, 23 Nov 2021 04:34:01 GMT` — nearly five years old, predating
    musl 1.2.4 and the current Alpine toolchain.
  - **Unversioned rolling URL**: `https://musl.cc/aarch64-linux-musl-cross.tgz`
    has no version tag; acquisition-time sha256+date would be the only pin,
    with no reproducibility.
  - **Cross-compile alone cannot produce native execution proof**: even if
    the tarball built `pi`, it cannot execute the aarch64 Bun sidecar or
    runtime on an x86_64 host without QEMU, which is excluded.
- Any future C attempt must still finish execution on `ubuntu-24.04-arm` and
  would need a vendor-published checksum or a content-addressed pin with a
  fresh acquisition date.

### QEMU

- Status: **excluded**.
- The bakeoff workflow contains no `docker/setup-qemu-action` and no binfmt
  setup.
- Any QEMU-assisted transcript elsewhere is contingency-only and cannot enter
  the native verdict table.

---

## 5. Exact pins

### x86_64 (executed 2026-08-27)

| Artifact | URL | sha256 | Acquired UTC | Vendor verification |
|---|---|---|---|---|
| `musl-dev_1.2.4-2_amd64.deb` | `http://archive.ubuntu.com/ubuntu/pool/universe/m/musl/musl-dev_1.2.4-2_amd64.deb` | `4b451ecb6a0f8469883058cf22a807f3bd9cc16d115cc08b7efc35fe8eb44db2` | 2026-08-26T18:59:42Z | Ubuntu noble universe Packages index sha256 |
| `musl-tools_1.2.4-2_amd64.deb` | `http://archive.ubuntu.com/ubuntu/pool/universe/m/musl/musl-tools_1.2.4-2_amd64.deb` | `46c01d212d3eb3a1322693089037f0a5c92383a089d39c392db3c86c19ffb229` | 2026-08-26T18:59:42Z | Ubuntu noble universe Packages index sha256 |
| `alpine-minirootfs-3.24.1-x86_64.tar.gz` | `https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/alpine-minirootfs-3.24.1-x86_64.tar.gz` | `41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081` | 2026-08-26T19:00:01Z | Alpine vendor `.sha256` sidecar (re-verified 2026-08-27) |
| `libstdc++-15.2.0-r5.apk` (x86_64) | `https://dl-cdn.alpinelinux.org/alpine/v3.24/main/x86_64/libstdc++-15.2.0-r5.apk` | `14c987b556f5385a5db18376e788c75f37d85321b8dc1920d926ea7daac1d6f6` | 2026-08-26T19:00:02Z | content-addressed SHA256 pin + acquisition date; `.PKGINFO` asserted; no signature verification |
| `libgcc-15.2.0-r5.apk` (x86_64) | `https://dl-cdn.alpinelinux.org/alpine/v3.24/main/x86_64/libgcc-15.2.0-r5.apk` | `393dcd32629f06d7d85409c272d142d0c082772d10b87ef55ee82f47de3be637` | 2026-08-26T19:00:02Z | content-addressed SHA256 pin + acquisition date; `.PKGINFO` asserted; no signature verification |
| `bun-linux-x64-musl-baseline.zip` | `https://github.com/oven-sh/bun/releases/download/bun-v<BUN_RUNTIME_VERSION>/bun-linux-x64-musl-baseline.zip` (version in [compatibility.md](compatibility.md)) | `56a7d6806cf155536c0178f0ea5fbd098e684fa509ebdb4fc0a7e19fb65382dc` | 2026-08-26T19:00:03Z | Bun runtime `SHASUMS256.txt` (re-verified 2026-08-27) |

### aarch64 (pins verified by download 2026-08-27; build+smoke pending REL-T3 CI)

| Artifact | URL | sha256 | Acquired UTC | Vendor verification |
|---|---|---|---|---|
| `alpine-minirootfs-3.24.1-aarch64.tar.gz` | `https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/aarch64/alpine-minirootfs-3.24.1-aarch64.tar.gz` | `f55a90f69052c5bd6f92cb09a8f47065970830b194c917a006fb94028e721259` | 2026-08-26T19:00:55Z | Alpine vendor `.sha256` sidecar (re-verified 2026-08-27) |
| `libstdc++-15.2.0-r5.apk` (aarch64) | `https://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64/libstdc++-15.2.0-r5.apk` | `2302e766d4e4926038ec166ecb85837ee884576115236ddb565e3a5fca4a11d7` | 2026-08-26T19:00:55Z | content-addressed SHA256 pin + acquisition date; `.PKGINFO` asserted; no signature verification |
| `libgcc-15.2.0-r5.apk` (aarch64) | `https://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64/libgcc-15.2.0-r5.apk` | `369aaa6e9d099a737bad6dd3e6c2fe7bb1547ca26d22b94ee0411228f709b403` | 2026-08-26T19:00:56Z | content-addressed SHA256 pin + acquisition date; `.PKGINFO` asserted; no signature verification |
| `bun-linux-aarch64-musl.zip` | `https://github.com/oven-sh/bun/releases/download/bun-v<BUN_RUNTIME_VERSION>/bun-linux-aarch64-musl.zip` (version in [compatibility.md](compatibility.md)) | `b98e0ad3625c5c00d1d5b5ff55605c7adddbfae151861e68ade57b2d3b8703bb` | 2026-08-26T19:01:03Z | Bun runtime `SHASUMS256.txt` (re-verified 2026-08-27) |

### aarch64 Candidate B pins (source-derived; conditional backup only)

| Artifact | Pin / version | Vendor verification |
|---|---|---|
| Zig `0.16.0` aarch64-linux | `ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17` | ziglang.org `download/index.json` shasum (re-verified 2026-08-27) |
| `cargo-zigbuild` `0.23.0` | `68c7df45b9d9934aaed5987fbf422b31419f81827b13a52251a61e1e772c6ff7` | crates.io API + static.crates.io download (re-verified 2026-08-27; not yanked) |

### Toolchain pins (both arches)

| Artifact | Pin / version | Vendor verification |
|---|---|---|
| Rust toolchain | pinned version (see [compatibility.md](compatibility.md)) via `dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4` | action SHA pin reused from `release-verification.yml` |
| Bun toolchain | pinned version (see [compatibility.md](compatibility.md)) via `oven-sh/setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6` | action SHA pin reused from `release-verification.yml` |
| `actions/checkout` | `34e114876b0b11c390a56381ad16ebd13914f8d5` | reused from `release-verification.yml` |
| `actions/upload-artifact` | `ea165f8d65b6e75b540449e92b4886f43607fa02` | reused from `release-verification.yml` |
| Ubuntu musl packages | `musl-dev=1.2.4-2`, `musl-tools=1.2.4-2` | exact apt versions; fail on drift |

The ledger covers **every explicit direct download managed by this recipe**
only. Each such download flows through a SHA256 assertion and writes one row:

`artifact`, `url`, `sha256` (computed locally), `acquired_utc`, `vendor_verification`

into `pins-ledger.tsv`. Expected direct-download rows:

| Arch / candidate | Required ledger artifact names | Count |
|---|---|---|
| x86_64 Candidate A | musl-dev deb, musl-tools deb, minirootfs tarball, `libstdc++-15.2.0-r5.apk`, `libgcc-15.2.0-r5.apk`, Bun musl runtime zip | 6 |
| aarch64 Candidate A | same class of four artifacts for aarch64 | 4 |
| aarch64 Candidate B | Candidate A set + Zig tarball + `cargo-zigbuild-*.crate` | 6 |

For the Alpine `libstdc++` / `libgcc` APKs, the last column records a
**content-addressed SHA256 pin + acquisition date** (plus `.PKGINFO`
assertion). It must **not** claim signature verification or a
vendor-published APK digest — live probing showed Alpine 3.24.1 minirootfs
keys reject the current v3.24 packages.

### Residual-risk boundary (SEC-002 disposition)

This prototype **keeps standard package-manager / action trust**. Outside the
direct-artifact ledger, and intentionally not replaced here:

- Cargo lock integrity (`cargo build --locked` / `cargo install --locked`)
- Bun lock integrity (`bun install --frozen-lockfile`)
- apt repository signatures for `musl-tools` / `musl-dev`
- commit-pinned setup actions (`actions/checkout`, `dtolnay/rust-toolchain`,
  `oven-sh/setup-bun`, `actions/upload-artifact`)

Those channels are residual risk accepted for this bakeoff; the ledger and
runtime completeness gate bound only recipe-managed direct downloads.

---

## 6. x86_64 executed evidence

All evidence below was produced by running `target/rel-r1-local/verify.sh`
on 2026-08-27 against a local Ubuntu 26.04 x86_64 host with a relocated
musl prefix (no root access). The full evidence tree is at
`target/rel-r1-local/evidence/`.

### Static gate

```
$ readelf -d target/x86_64-unknown-linux-musl/release/pi
(20 dynamic entries; zero NEEDED tags)

$ file target/x86_64-unknown-linux-musl/release/pi
ELF 64-bit LSB pie executable, x86-64, static-pie linked, stripped

$ ldd target/x86_64-unknown-linux-musl/release/pi
	statically linked
ldd_exit=0
```

Note: Ubuntu 24.04 (CI runner) `ldd` prints "not a dynamic executable" for
static-pie binaries; Ubuntu 26.04 (local host) prints "statically linked".
Both are valid proofs. The verification script accepts either form.

### Loader resolution (staged)

```
$ ld-musl-x86_64.so.1 --library-path $USERLAND/usr/lib --list stage/pi-extension-host
	/lib/ld-musl-x86_64.so.1 (0x...)
	libstdc++.so.6 => $USERLAND/usr/lib/libstdc++.so.6 (0x...)
	libc.musl-x86_64.so.1 => /lib/ld-musl-x86_64.so.1 (0x...)
	libgcc_s.so.1 => $USERLAND/usr/lib/libgcc_s.so.1 (0x...)

$ ld-musl-x86_64.so.1 --library-path $USERLAND/usr/lib --list stage/bun
	(same resolution pattern)
```

Isolation control (default paths, no `--library-path`):

```
$ ld-musl-x86_64.so.1 --list stage/pi-extension-host
Error loading shared library libstdc++.so.6: No such file or directory
Error relocating ... symbol not found
exit 127
```

### PT_INTERP (staged + unpacked)

Both `pi-extension-host` and `bun` have exactly one
`[Requesting program interpreter: /lib/ld-musl-x86_64.so.1]` — confirmed for
staged and unpacked copies.

### Hello protocol smoke (unpacked)

```
$ ./pi --version
0.1.0

$ hello-compiled-response.jsonl
{"id":1,"kind":"res","method":"hello","payload":{"protocolVersion":1,"compatibilityVersion":"0.80.10"}}

$ hello-runtime-response.jsonl
{"id":1,"kind":"res","method":"hello","payload":{"protocolVersion":1,"compatibilityVersion":"0.80.10"}}
```

Both acks validated by the Python field-exact checker (all 5 fields match).

### Ledger gate

```
selected_candidate=A
expected_count=6
expected_names=musl-dev_1.2.4-2_amd64.deb musl-tools_1.2.4-2_amd64.deb alpine-minirootfs-3.24.1-x86_64.tar.gz libstdc++-15.2.0-r5.apk libgcc-15.2.0-r5.apk bun-linux-x64-musl-baseline.zip
```

### Verdict

```
$ cat evidence/native-verdict.txt
PASS
```

---

## 7. aarch64 source-derived legs

The aarch64 recipe is identical to x86_64 Candidate A with arch-specific
names. Primary-source verification performed 2026-08-27:

| Source | Check | Result |
|---|---|---|
| Launchpad API | noble arm64 `musl-dev` 1.2.4-2 | **Published** |
| Launchpad API | noble arm64 `musl-tools` 1.2.4-2 | **Published** |
| Alpine aarch64 APKINDEX | `libstdc++` version | `15.2.0-r5`, arch `aarch64` |
| Alpine aarch64 APKINDEX | `libgcc` version | `15.2.0-r5`, arch `aarch64` |
| Alpine vendor `.sha256` | aarch64 minirootfs | matches pin `f55a90f6...` |
| Bun `SHASUMS256.txt` | `bun-linux-aarch64-musl.zip` | matches pin `b98e0ad3...` |
| ziglang.org `index.json` | Zig 0.16.0 aarch64-linux shasum | matches pin `ea4b09bf...` |
| crates.io API | cargo-zigbuild 0.23.0 checksum | matches pin `68c7df45...`; not yanked |
| GitHub runner-images README | `ubuntu-24.04-arm` | available (arm64) |

All four aarch64 direct-download artifacts were downloaded and sha256-verified
on the x86_64 host. Build and smoke execution is pending REL-T3 CI on
`ubuntu-24.04-arm`.

### musl.cc probe (Candidate C rejection evidence)

```
$ curl -sI https://musl.cc/aarch64-linux-musl-cross.tgz
HTTP/1.1 200 OK
Content-Length: 108096828
Last-Modified: Tue, 23 Nov 2021 04:34:01 GMT

$ curl -s https://musl.cc/sha256sum.txt
HTTP/1.1 404 Not Found

$ curl -s https://musl.cc/SHA256SUMS
HTTP/1.1 404 Not Found
```

---

## 8. REL-T3 handoff notes — RESOLVED

Two latent defects were found in the unexecuted `musl-bakeoff.yml` workflow
script during local execution. REL-T3 must fix these before relying on the CI
form: **all four are fixed** (commit `0767ae6`, plus execution-discovered
defects 5-6 in `abc64a7` and pinned-reference provisioning in `daeb68d`);
each fix is proven by the executed evidence in §11, not by inspection alone:

1. **`.PKGINFO` format**: The workflow's `grep -Fx "pkgname=$package"` uses
   `key=value` but Alpine `.PKGINFO` uses `key = value` (with spaces). Fix:
   `grep -Fx "pkgname = $package"`. Same for `pkgver` and `arch`.

2. **Bash regex quoting**: The `assert_musl_resolution` function uses
   `[[ "$line" =~ ^[[:space:]]*(/[^[:space:](]+) ]]` which fails on bash 5.3+
   with "unexpected token" because the unquoted regex contains `(`. Fix:
   assign the regex to a variable first:
   `local re='^[[:space:]]*(/[^[:space:](]+)'; [[ "$line" =~ $re ]]`.

3. **`ldd` output variance**: The workflow's `grep -Fq 'not a dynamic
   executable'` fails on systems where `ldd` prints "statically linked" for
   static-pie binaries. Fix: accept either form with
   `grep -Eq 'not a dynamic executable|statically linked'`.

4. **Bun `--outfile` path resolution**: When `bun build --compile --outfile`
   is given an absolute path while `cwd` is the package directory, Bun may
   concatenate the cwd with the outfile path. Use a relative path from the
   package cwd (e.g. `--outfile ../../target/.../pi-extension-host`).

---

## 9. Native verdict

| Arch | Candidate A | Candidate B | Candidate C | QEMU | Verdict |
|---|---|---|---|---|---|
| x86_64 | **PASS** (executed locally 2026-08-27; re-executed in a clean container by REL-T3 2026-08-27, §11) | not in scope | deleted | excluded | **PASS** |
| aarch64 | ARM-native; pending CI (billing-locked, §11) | **CROSS-PASS** — build+static+provision+pins executed by REL-T3 2026-08-27, §11; native smoke pending ARM64 | deleted | excluded | **CROSS-PASS / native smoke PENDING ARM64** |

---

## 10. Primary sources

- Issue #102 (REL-R1) and issue #112 (REL-T3)
- `.github/workflows/release-verification.yml` — action SHA pins reused here
- `scripts/release/host.ts` — compiled / runtime-bundle argv and hello ack contract
- Bun runtime `SHASUMS256.txt` (version in [compatibility.md](compatibility.md); re-verified 2026-08-27)
- Alpine v3.24.1 release directories for `x86_64` / `aarch64` (minirootfs vendor
  `.sha256`); Alpine `libstdc++` / `libgcc` `15.2.0-r5` APKs pinned by
  committed content SHA256 (no APKINDEX / apk-signature path — minirootfs keys
  reject current v3.24 packages as `UNTRUSTED signature`)
- Alpine aarch64 `APKINDEX` (fetched 2026-08-27) — confirms `15.2.0-r5` for
  `libstdc++` and `libgcc` on `aarch64`
- Ubuntu noble `musl-tools` / `musl-dev` `1.2.4-2` — Packages index sha256
  (x86_64) and Launchpad API (arm64 Published)
- https://ziglang.org/download/index.json (re-verified 2026-08-27)
- crates.io API `cargo-zigbuild` 0.23.0 (re-verified 2026-08-27)
- musl `ldso/dynlink.c` path-file replacement semantics
- GitHub runner-images README — `ubuntu-24.04-arm` availability
- musl.cc HTTP probe — no checksums (404), stale tarball (2021-11-23)

---

## 11. REL-T3 execution evidence (2026-08-27)

Commits on `feat/ver-align-canonical-pin`: `0767ae6` (handoff defects 1-4),
`699f4db` (rm -rf guards), `abc64a7` (execution-discovered defects 5-6),
`daeb68d` (pinned TypeScript reference provisioning). Independent review:
CLEAN (no Critical/Important/Minor findings).

### CI dispatch — blocked externally

`gh workflow run musl-bakeoff.yml --ref feat/ver-align-canonical-pin` → run
33054055228 (2026-08-27T08:26:40Z) failed in 8s: "The job was not started
because your account is locked due to a billing issue" — the same external
blocker REL-R1 recorded (run 32986086665). Re-dispatch after unlock.

### x86_64 — EXECUTED in a clean ubuntu:24.04 container (Candidate A, PASS)

The fixed workflow's x86_64 run blocks (init evidence / candidate A /
acceptance) were extracted verbatim from `daeb68d` and executed in
`docker run ubuntu:24.04`. Only adaptations: Actions → sha256-verified
rustup-init + Bun 1.3.14 SHASUMS256.txt toolchain provisioning (ledgered),
`sudo` dropped (container root), `RUNNER_TEMP→/tmp/runner`,
`GITHUB_WORKSPACE→/work`, `CARGO_BUILD_JOBS=8` (4-vCPU runner shape).

- `apt musl-dev/musl-tools 1.2.4-2` pinned install; packages.tsv recorded.
- `cargo build -p pi --release --locked --target x86_64-unknown-linux-musl`
  exit 0 (aws-lc-sys, zstd-sys compiled clean under musl-gcc); 7m24s cold.
- Static gate: `readelf -d` zero NEEDED entries; `file` → "static-pie
  linked, stripped"; `ldd` → "statically linked" (defect-3 fix proven).
- Alpine minirootfs 3.24.1 + libstdc++/libgcc 15.2.0-r5 APKs sha256-verified;
  spaced `.PKGINFO` asserts passed (defect-1 fix proven); loader installed
  to `/lib/ld-musl-x86_64.so.1`; `/etc/ld-musl-x86_64.path` → isolated
  userland only.
- Sidecar built with relative `--outfile ../../$STAGE/pi-extension-host`
  (defect-4 fix proven); staged+unpacked PT_INTERP exactly
  `/lib/ld-musl-x86_64.so.1` for sidecar and Bun runtime; loader `--list`
  isolation gates passed (defect-2 fix exercised).
- Hello protocol smokes (unpacked): the compiled sidecar and `./bun
  pi-extension-host.js` both acked
  `{"id":1,"kind":"res","method":"hello","payload":{"protocolVersion":1,"compatibilityVersion":"0.80.10"}}`.
- `./pi --version` → `0.1.0`.
- Ledger gate: 4/4 direct downloads (minirootfs, 2 APKs, Bun zip), each
  sha256 + `acquired_utc` recorded; toolchain ledger (rustup-init,
  bun-linux-x64.zip) sha256-verified against vendor sidecars.
- Verdict: `PASS`.

### aarch64 — CROSS-PASS in a clean ubuntu:24.04 container (Candidate B)

Candidate A is ARM-native by construction (issue #112: "apt musl-tools on
ubuntu-24.04-arm"); the x86_64 host executed the issue-sanctioned REL-R1
backup: cargo-zigbuild 0.23.0 + Zig 0.16.0, both sha256-pinned (Zig via the
host-appropriate `zig-x86_64-linux-0.16.0.tar.xz` archive of the same
pinned version; shasum from ziglang.org `download/index.json`, acquired
2026-08-27 — the workflow's aarch64 archive pin `ea4b09bf…` re-verified
against the same index).

- `cargo zigbuild -p pi --release --locked --target
  aarch64-unknown-linux-musl` exit 0 (aws-lc-sys, zstd cross-built via zig);
  `cargo-zigbuild --version` → `cargo-zigbuild 0.23.0` (defect-5 fix).
- Static gate: `readelf -d` zero NEEDED; `file` → "ARM aarch64 … statically
  linked, stripped"; `ldd` → "not a dynamic executable".
- Userland: aarch64 minirootfs + APK pins verified; spaced `.PKGINFO`
  asserts passed; loader copied to `/lib/ld-musl-aarch64.so.1`;
  `/etc/ld-musl-aarch64.path` → isolated userland.
- Sidecar cross-compiled from the x64 Bun toolchain with `--target
  bun-linux-arm64-musl`; staged+unpacked PT_INTERP exactly
  `/lib/ld-musl-aarch64.so.1` for sidecar and Bun runtime.
- Ledger gate: 6/6 (minirootfs, 2 APKs, Bun zip, Zig tarball, zigbuild crate).
- Skips (structurally impossible on x86_64; recorded in
  `native-execution-skips.txt`): loader `--list`, both hello smokes —
  they require native ARM64; QEMU is excluded by §QEMU doctrine; Actions
  runners are billing-locked (run 33054055228).
- Verdict: `CROSS-PASS (build+static+provision+pins; native execution
  pending ARM64)`.
