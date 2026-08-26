# REL-R1 — musl toolchain & userland bakeoff (decision + evidence contract)

Stable ID `REL-R1`, issue metaphorics/pi-oxidized#102. This document is the
current decision contract for the temporary
`.github/workflows/musl-bakeoff.yml` workflow. It records which candidate is
expected to win, the exact winner-selection rule, the rejection / contingency
reasons for the backups, and the artifact / pin evidence the workflow must
upload. It does **not** claim native pass/fail results that have not been
observed.

Authored 2026-08-26. Pin values below are grounded from primary sources on
that date (`agent://PlanRelR1`, Bun `SHASUMS256.txt`, Alpine release sidecars,
Ubuntu noble package pages, ziglang.org `download/index.json`, crates.io).

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

Native verdict status: **PENDING** an actual `workflow_dispatch` run of
`.github/workflows/musl-bakeoff.yml` on both `ubuntu-24.04` and
`ubuntu-24.04-arm`. No native pass or fail result is asserted here.

---

## 2. Winner-selection rule

Winner **per architecture** = the first candidate that, **natively** on that
architecture's runner, passes every gate below:

1. `cargo build -p pi --release --locked --target <musl-triple>` exits 0
2. `readelf -d` on `pi` shows **zero** `NEEDED` entries
3. `ldd` on `pi` prints `not a dynamic executable`
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
    before PASS (x86_64 / aarch64 Candidate A: minirootfs + libstdc++ +
    libgcc + Bun runtime asset; aarch64 Candidate B adds Zig +
    cargo-zigbuild crate)

Rules:

- Expected candidate on both arches: **Candidate A**.
- Candidate B is tried **only on aarch64**, and **only if Candidate A build
  fails**. Candidate A evidence is retained regardless.
- Candidate C is **not executed** by this workflow. It remains a documented
  contingency only.
- QEMU / `docker/setup-qemu-action` is **not present** and must never count as
  native evidence.
- If Candidate A later passes on both arches, both backups lose. Before
  REL-T3, losing backup command blocks are deleted from the recipe; their
  failure / not-tried record remains in the evidence table.

---

## 3. Current expected candidate

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

Userland recipe (both arches, with arch-specific names):

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
   `usr/lib` into `$USERLAND_DIR`. Ledger text must say content-addressed SHA
   pin + acquisition date; do **not** claim signature verification or a
   vendor-published APK digest.
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
- `payload.compatibilityVersion == "0.80.10"`

All three smoke commands (`pi --version`, compiled hello, runtime hello) run
only against a freshly unpacked archive, with cwd inside the unpack directory,
and only after unpacked exact `PT_INTERP` equality to `/lib/$MUSL_LOADER` and
`ld-musl --list` isolation proofs both pass under the strict path file.

---

## 4. Rejection / contingency reasons

### Candidate B — `cargo-zigbuild` on native `ubuntu-24.04-arm`

- Status in this workflow: **conditional backup only**.
- Trigger: Candidate A provisioning or cargo build fails on aarch64.
- Pins resolved for execution:
  - Zig `0.16.0` aarch64-linux tarball sha256
    `ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17`
  - `cargo-zigbuild` `0.23.0` crate sha256
    `68c7df45b9d9934aaed5987fbf422b31419f81827b13a52251a61e1e772c6ff7`
- Must unset `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER` and
  `CC_aarch64_unknown_linux_musl`; a `-C linker=` value opts cargo-zigbuild out
  of Zig.
- Rejection reason if unused: Candidate A already satisfied the native gates.
- Contingency reason if used: apt musl / musl-gcc path failed on aarch64 while
  a self-contained Zig musl toolchain remains available natively.

### Candidate C — musl.cc `aarch64-linux-musl-cross.tgz`

- Status in this workflow: **not executed**.
- Contingency only if the arm runner is unavailable or A and B both fail.
- Rejection / non-use reasons now:
  - no vendor checksums or signatures on musl.cc;
  - unversioned rolling URL (acquisition-time sha256+date would be the only pin);
  - cross-compile alone cannot produce native execution proof.
- Any future C attempt must still finish execution on `ubuntu-24.04-arm`.

### QEMU

- Status: **excluded**.
- The bakeoff workflow contains no `docker/setup-qemu-action` and no binfmt
  setup.
- Any QEMU-assisted transcript elsewhere is contingency-only and cannot enter
  the native verdict table.

---

## 5. Exact pins visible in the workflow

| Artifact | Pin / version | Vendor verification |
|---|---|---|
| Rust toolchain | `1.97.1` via `dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4` | action SHA pin reused from `release-verification.yml` |
| Bun toolchain | `1.3.14` via `oven-sh/setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6` | action SHA pin reused from `release-verification.yml` |
| `actions/checkout` | `34e114876b0b11c390a56381ad16ebd13914f8d5` | reused from `release-verification.yml` |
| `actions/upload-artifact` | `ea165f8d65b6e75b540449e92b4886f43607fa02` | reused from `release-verification.yml` |
| Ubuntu musl packages | `musl-dev=1.2.4-2`, `musl-tools=1.2.4-2` | exact apt versions; fail on drift |
| `bun-linux-x64-musl-baseline.zip` | `56a7d6806cf155536c0178f0ea5fbd098e684fa509ebdb4fc0a7e19fb65382dc` | Bun v1.3.14 `SHASUMS256.txt` (2026-08-26) |
| `bun-linux-aarch64-musl.zip` | `b98e0ad3625c5c00d1d5b5ff55605c7adddbfae151861e68ade57b2d3b8703bb` | Bun v1.3.14 `SHASUMS256.txt` (2026-08-26) |
| `alpine-minirootfs-3.24.1-x86_64.tar.gz` | `41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081` | Alpine vendor `.sha256` (2026-08-26) |
| `alpine-minirootfs-3.24.1-aarch64.tar.gz` | `f55a90f69052c5bd6f92cb09a8f47065970830b194c917a006fb94028e721259` | Alpine vendor `.sha256` (2026-08-26) |
| `libstdc++-15.2.0-r5.apk` (x86_64) | `14c987b556f5385a5db18376e788c75f37d85321b8dc1920d926ea7daac1d6f6` | content-addressed SHA256 pin + acquisition date; `.PKGINFO` asserted; no signature verification |
| `libgcc-15.2.0-r5.apk` (x86_64) | `393dcd32629f06d7d85409c272d142d0c082772d10b87ef55ee82f47de3be637` | content-addressed SHA256 pin + acquisition date; `.PKGINFO` asserted; no signature verification |
| `libstdc++-15.2.0-r5.apk` (aarch64) | `2302e766d4e4926038ec166ecb85837ee884576115236ddb565e3a5fca4a11d7` | content-addressed SHA256 pin + acquisition date; `.PKGINFO` asserted; no signature verification |
| `libgcc-15.2.0-r5.apk` (aarch64) | `369aaa6e9d099a737bad6dd3e6c2fe7bb1547ca26d22b94ee0411228f709b403` | content-addressed SHA256 pin + acquisition date; `.PKGINFO` asserted; no signature verification |
| Zig (Candidate B only) | `0.16.0` aarch64-linux | ziglang.org index shasum (2026-08-26) |
| cargo-zigbuild (Candidate B only) | `0.23.0` | crates.io checksum (2026-08-26) |

The ledger covers **every explicit direct download managed by this recipe**
only. Each such download flows through a SHA256 assertion and writes one row:

`artifact`, `url`, `sha256` (computed locally), `acquired_utc`, `vendor_verification`

into `pins-ledger.tsv`. Expected direct-download rows before PASS:

| Arch / candidate | Required ledger artifact names | Count |
|---|---|---|
| x86_64 Candidate A | minirootfs tarball, `libstdc++-15.2.0-r5.apk`, `libgcc-15.2.0-r5.apk`, Bun musl runtime zip | 4 |
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

## 6. Exact artifacts the workflow uploads

Both jobs upload under `if: always()`:

| Job | Runner | Artifact name | Path |
|---|---|---|---|
| Native x86_64 candidate A | `ubuntu-24.04` | `rel-r1-bakeoff-x86_64` | `target/rel-r1-bakeoff/x86_64` |
| Native aarch64 candidates A then B | `ubuntu-24.04-arm` | `rel-r1-bakeoff-aarch64` | `target/rel-r1-bakeoff/aarch64` |

Required evidence contents (created as far as the failing step allows):

- `environment.txt` — runner / toolchain / image identity
- `pins-ledger.tsv` — ledger of every explicit direct download managed by this recipe
- `direct-download-ledger-gate.txt` — selected candidate + expected names/count asserted before PASS
- `candidate-A/` — exact command, packages/provenance, attempt log
- `candidate-B/` — aarch64 only; exact command, zig/cargo-zigbuild versions, attempt log when attempted
- `selected-candidate.txt` — `A` or `B` when a build candidate succeeds
- `pi-readelf-dynamic.txt`, `pi-file.txt`, `pi-ldd.txt` — static gate transcripts
- `ld-musl.path` — installed path-file contents (exactly `$USERLAND_DIR/usr/lib`)
- `host-readelf-dynamic.txt`
- `host-interpreter.txt`, `runtime-interpreter.txt` — staged exact `PT_INTERP` proofs (`/lib/$MUSL_LOADER`)
- `host-loader-resolution.txt`, `runtime-loader-resolution.txt` — staged strict isolation proofs
- `unpacked-host-interpreter.txt`, `unpacked-runtime-interpreter.txt` — unpacked exact `PT_INTERP` proofs before smoke
- `unpacked-host-loader-resolution.txt`, `unpacked-runtime-loader-resolution.txt` — unpacked strict isolation proofs before smoke
- `downloads/*.PKGINFO` — asserted pkgname/pkgver/arch records
- `bakeoff.tar.gz`, `archive-members.txt`
- `pi-version.txt`
- `hello-compiled-request.jsonl` / `hello-compiled-response.jsonl`
- `hello-runtime-request.jsonl` / `hello-runtime-response.jsonl`
- `acceptance.log`
- `native-verdict.txt` — written only after all gates pass

---

## 7. Native verdict (explicitly pending)

| Arch | Candidate A | Candidate B | Candidate C | QEMU | Native verdict |
|---|---|---|---|---|---|
| x86_64 | expected; not yet run | not in scope | not executed | excluded | **PENDING workflow run** |
| aarch64 | expected; not yet run | conditional on A build failure; not yet run | not executed | excluded | **PENDING workflow run** |

This document will be updated with observed pass/fail cells only after the
uploaded `rel-r1-bakeoff-*` artifacts from a real `workflow_dispatch` run are
inspected. Until then, any claim that a candidate already passed or failed
natively is out of scope and unsupported.

---

## 8. Primary sources

- Issue #102 (REL-R1) and plan `agent://PlanRelR1`
- `.github/workflows/release-verification.yml` — action SHA pins reused here
- `scripts/release/host.ts` — compiled / runtime-bundle argv and hello ack contract
- Bun v1.3.14 `SHASUMS256.txt`
- Alpine v3.24.1 release directories for `x86_64` / `aarch64` (minirootfs vendor
  `.sha256`); Alpine `libstdc++` / `libgcc` `15.2.0-r5` APKs pinned by
  committed content SHA256 (no APKINDEX / apk-signature path — minirootfs keys
  reject current v3.24 packages as `UNTRUSTED signature`)
- Ubuntu noble `musl-tools` / `musl-dev` `1.2.4-2`
- https://ziglang.org/download/index.json
- crates.io `cargo-zigbuild` 0.23.0 checksum
- musl `ldso/dynlink.c` path-file replacement semantics
