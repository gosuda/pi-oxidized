# PAR-MATH-RESEARCH: T4 LaTeX math rendering strategy (decision record)

| Field | Value |
| --- | --- |
| Issue | [#36][issue-36], `PAR-MATH-RESEARCH` (research) |
| Decision type | recorded decision, not implementation |
| Deliverable | this document plus the T4 evidence cell in `docs/PARITY_LEDGER.md`, and only these |
| Decision | Port the upstream pi-tui LaTeX renderer (`.references/pi/packages/tui/src/latex.ts` plus the markdown math hooks in `.references/pi/packages/tui/src/components/markdown.ts`) as a Rust-native layout engine inside `pi-tui`; reject the reduced TeX→unicode tier, the embedded JavaScript engine, the third-party crate, and the external process. |

[issue-36]: https://github.com/metaphorics/pi-oxidized/issues/36

## Scoring against the issue's three criteria

| Option | Coverage of transcript-emitted math | No-JS dependency cost | pi-tui product-agnosticism | Verdict |
| --- | --- | --- | --- | --- |
| Rust-native layout engine (selected) | Full: the upstream renderer's own session corpus — stacked fractions, operator limits, matrices, cases — ports as-is (`test/latex.test.ts` describes at lines 16, 61, 93, 113, 167) | Zero: reuses `visible_width` and the pinned `unicode-width`/`unicode-segmentation` crates | Holds: pure function over `&str` in `pi-tui`, no product knowledge | Selected |
| Reduced TeX→unicode tier | Fails: the corpus's display fractions, `\lim` stacking, matrix and cases layouts have no unicode-linear form | Zero | Holds | Rejected |
| Embedded JavaScript engine (KaTeX/MathJax under quickjs or similar) | Exceeds, but invisibly to a terminal cell grid | Fails: a JS runtime in the native binary and its dependency tree | Fails: a runtime dependency with product-scale weight inside the product-agnostic crate | Rejected |
| Third-party Rust crate | No maintained crate implements this terminal-unicode contract (stacked layouts, PUA markers, raw fallback) | New workspace dependency | Fails the five-crate boundary hygiene | Rejected |
| External process (LaTeX toolchain) | Exceeds | Fails: installs a toolchain at runtime | Fails: `pi-tui` spawns nothing | Rejected |

## Settled observable contract

### Command tables (the supported TeX subset)

All counts verified against the pinned reference tree. Port must be entry-exact: same names, same mappings, same line-count sanity.

| Table | Entries | Reference | Shape |
| --- | --- | --- | --- |
| `SYMBOLS` | 217 | `latex.ts:3-221` | command → unicode glyph (Greek, binary operators, relations, arrows, dots, delimiters) |
| `NAMED_OPERATORS` | 32 | `latex.ts:223-256` | set; rendered with deferred operator spacing |
| `LIMIT_OPERATORS` | 11 | `latex.ts:258-270` | set; may take stacked limits in display mode |
| `DISPLAY_LIMIT_SYMBOLS` | 16 | `latex.ts:272-289` | set; big operators that stack limits in display mode |
| `RELATION_COMMANDS` | 81 | `latex.ts:291-373` | set; symbol emitted with surrounding spaces |
| `NEGATED_SYMBOLS` | 30 | `latex.ts:375-406` | glyph → negated glyph, driven by `\not` |
| `BLACKBOARD` | 7 | `latex.ts:408-416` | letter → blackboard letter, driven by `\mathbb` |
| `SUPERSCRIPTS` | 40 | `latex.ts:418-459` | char → superscript char (digits, `+ - = ( )`, `a-z` minus `q`) |
| `SUBSCRIPTS` | 32 | `latex.ts:461-494` | char → subscript char (digits, `+ - = ( )`, supported letters) |
| `SPACING_COMMANDS` | 12 | `latex.ts:496-509` | set; each renders one space |
| `NEGATIVE_SPACING_COMMANDS` | 4 | `latex.ts:510` | set; trims trailing output instead of emitting |
| `IGNORED_COMMANDS` | 6 | `latex.ts:512-519` | set; style-mode commands rendered as empty |
| `SIZE_COMMANDS` | 12 | `latex.ts:520-533` | set; `\big`-family rendered as empty |
| `PLAIN_WRAPPERS` | 30 | `latex.ts:534-565` | set; argument rendered plain (`text*`/`mbox` keep inner spaces, rest trim) |
| `ACCENTS` | 18 | `latex.ts:566-585` | command → combining mark; single-char argument appends the mark, longer argument falls back to `cmd(...)` |

Commands handled outside the tables (`latex.ts:914-1086`): escaped punctuation `\{ \} \$ \% \# \_ \&`, `\|` → `‖`, `\\` (row split, optional `[dimen]` consumed by `splitEnvironmentRows`, `latex.ts:1221-1223`), `\not`, `\left`/`\middle`/`\right` with optional `.` invisible delimiter, `\frac`/`\dfrac`/`\tfrac`, `\sqrt` with optional `[n]` (degrees 2, 3, 4 map to `√`, `∛`, `∜`; other degrees compose a superscript), `\boxed`/`\fbox` → `[...]`, `\binom`/`\dbinom`/`\tbinom` → `(...) choose ...`, `\operatorname` with optional `*`, `\mod`/`\bmod`, `\pmod`/`\pod`, `\overset`/`\stackrel`, `\underset`, `~` → space, bare `&` consumed, `=`/`<`/`>` spaced, whitespace collapsed to one space.

Parser invariants (`latex.ts:797-905`): a render succeeds only when the parser consumed the entire source and never set its unsupported flag. Unsupported inputs include unknown commands, stray or unclosed braces, `\end` outside an environment, and repeated sub/superscripts on one operator. Failed nested renders propagate the failure (`renderNested`, `latex.ts:1343-1350`).

Display-mode semantics (`latex.ts:1003-1017`, `1131-1143`): in display mode `\frac`/`\dfrac` stack vertically (never `\tfrac`, never nested under a non-stacking context) and `LIMIT_OPERATORS`/`DISPLAY_LIMIT_SYMBOLS` with scripts stack limits vertically; inline mode composes linearly with bracketed or script limits.

### Environments (24, `latex.ts:1225-1296`)

- Passive: `equation`, `equation*`, `displaymath` — body rendered nested and trimmed.
- Row-splitting: `aligned`, `align`, `align*`, `alignedat`, `alignat`, `alignat*`, `gather`, `gathered`, `multline`, `multline*`, `split` — rows split on `\\`, `&` cells joined (`alignat` family pairs cells and drops the leading column-spec group).
- Cases: `cases`, `cases*` — two columns, `⎧`/`⎨`/`⎩` braces by row index, `if`/`when`/`for`/`otherwise` conditions keep natural wording.
- Matrix: `array`, `matrix`, `smallmatrix` (bare) and `pmatrix` `⎛⎞⎜⎟⎝⎠`, `bmatrix` `⎡⎤⎢⎥⎣⎦`, `Bmatrix` `⎧⎫⎨⎬⎩⎭`, `vmatrix` `│`, `Vmatrix` `║` (delimiter table at `latex.ts:1317-1323`); columns padded with the protected space and joined with ` │ `.

Any other environment name sets unsupported → fallback. Multi-line matrices and stacked display layouts become layout nodes composed by `renderLayout` (`latex.ts:709-795`).

### PUA layout-marker protocol (U+F0000–U+F0005)

- `U+F0000`/`U+F0001` layout-marker start/end (`latex.ts:672-675`): in-band `<U+F0000>index<U+F0001>` references into the layout-node vector (fraction, operator, matrix); consumed and rewritten by `renderLayout`.
- `U+F0002` protected space (`latex.ts:676`): survives wrapping as intra-matrix column padding; converted to a real space at both return sites (`latex.ts:1369`, `latex.ts:1379`).
- `U+F0004`/`U+F0005` named-operator start/end (`latex.ts:627-630`): deferred operator spacing, resolved and stripped by `normalizeOutput` (`latex.ts:632-643`).
- `U+0000` negative-space sentinel (`latex.ts:511`) exists only inside the parser to trigger trailing-space trims; it is never emitted.

No-leak invariant: no codepoint in U+F0000–U+F0005 survives into a rendered string — layout markers are consumed by `renderLayout`, named-operator markers are stripped in `normalizeOutput`, and the protected space is mapped to a space at every return. The port keeps these as private constants and must uphold the invariant (no PUA byte reaches the terminal).

### Fallback contract

`render_latex` returns `None` for any unsupported or malformed input (unknown command, unknown environment, unbalanced braces, unconsumed trailing input, failed nested render). The markdown caller then emits the raw span verbatim: inline `renderLatex(text) ?? raw`, block `renderLatex(text, {display: true}) ?? raw.trim()` (`markdown.ts:645-653`, `markdown.ts:505-514`). Nothing is partially rendered: an expression either renders completely or reproduces its source bytes.

### Delimiter contract

Inline (`tokenizeInlineLatex`, `markdown.ts:52-99`): openers `$$…$$`, `\(…\)`, `\[…\]`, and single `$…$`. Single `$` is rejected — leaving the span as ordinary text — when any of four rules fires (`markdown.ts:72-82`): the inner text ends with whitespace; the character after the closing `$` is a digit (currency); the inner text is an ALL-CAPS identifier (optionally one trailing punctuation) followed by an identifier start (shell variables like `$HOME/`); or the inner text contains a backtick (code span). A `$` followed by whitespace never opens (`markdown.ts:64`). A closing delimiter preceded by an odd number of backslashes is escaped and skipped (`isEscaped`/`findClosingDelimiter`, `markdown.ts:32-46`). A matched inline span must be nonempty and single-line (`markdown.ts:93-95`).

Block (`tokenizeBlockLatex`, `markdown.ts:101-121`): only `$$…$$` and `\[…\]`, opener at at most three leading spaces (`^ {0,3}`), closer followed by optional blanks then a line end or end of input. The block-scan hint is `/(?:^|\n) {0,3}(?:\$\$|\\\[)/` (`markdown.ts:127-130`); the inline hint scans for the first `$`, `\(`, or `\[` (`markdown.ts:136-141`).

Exclusions: escaped openers (`\$`) are consumed as markdown escapes before the math path sees them; fenced code blocks and inline code spans are tokenized ahead of math, so math delimiters inside them stay literal (witness: `markdown.test.ts:929-934`).

Option gate: `renderLatex !== false`, default enabled (`markdown.ts:227-228`, `508`, `648`).

### Streaming pending behavior

While a delimiter is still open, the span renders raw, then re-renders once it closes (witness: `markdown.test.ts:955-967`):

- Inline: with no closer found, a pending token is produced only when the opener is `\(` or `\[`, or when the pending body passes `looksLikePendingDollarMath` — `/\\[A-Za-z]+|[_^=+*/<>()[\]|±≤≥≠≈∈→⇒∞∫∑√-]/` (`markdown.ts:48-50`, `84-90`).
- Block: a pending `\[` block is always pending; a pending `$$` block is pending only when its body passes `looksLikePendingDollarMath` (`markdown.ts:112-119`).

This keeps ordinary prose containing one `$` from flickering as math while an open `\(` or a math-shaped `$$` body holds its raw form across stream chunks.

## Dependency boundary

No JavaScript engine, no new crate, no external process. The port computes widths with the existing `visible_width` (`crates/pi-tui/src/text/width.rs:160`), already backed by the pinned `unicode-width =0.2.2` (`crates/pi-tui/Cargo.toml:25`) and `unicode-segmentation =1.13.3` (`crates/pi-tui/Cargo.toml:23`). `crates/pi-tui/Cargo.toml` remains unchanged; issue #37's "zero new pi-tui workspace deps" acceptance restates this.

## Tokenizer authority

`pulldown-cmark`'s `Options::ENABLE_MATH` is recorded as NOT the tokenizer authority for T4. The current parser options set only tables, strikethrough, and tasklists (`crates/pi-tui/src/components/markdown.rs:351-354`), and the current drop of `Event::InlineMath`/`Event::DisplayMath` (`crates/pi-tui/src/components/markdown.rs:457-463`) is the seam that wiring replaces — but the observable delimiter contract above (four single-`$` rejection rules, pending gating, escape and code exclusion) is the upstream marked-extension behavior, not pulldown-cmark's math grammar. The Rust path must implement the upstream contract directly, not delegate delimiter recognition to `ENABLE_MATH`.

## Implementation routing (PAR-MATH, issue #37)

1. New module `crates/pi-tui/src/latex.rs` with the seam `render_latex(source: &str, display: bool) -> Option<String>` — an observable port of `latex.ts` (tables, grammar, environments, PUA protocol, layout composer).
2. `crates/pi-tui/src/components/markdown.rs`: implement the delimiter and streaming contract at the tokenizer seam, replacing the `Event::InlineMath`/`Event::DisplayMath` drop at `markdown.rs:457-463`; render through `render_latex` with raw-span fallback.
3. `MarkdownOptions` (`crates/pi-tui/src/components/markdown.rs:154-163`) gains `render_latex: bool`, default `true`, gating both inline and block paths (parity with `markdown.ts:227-228`).
4. Corpus port: `.references/pi/packages/tui/test/latex.test.ts` (496 lines: 5 session-table describes plus 18 focused cases) and the `describe("LaTeX math")` block of `.references/pi/packages/tui/test/markdown.test.ts` (12 cases, lines 785–969) become the Rust unit corpus for `latex.rs` and the markdown math path.
5. T4 parity witness: host-tier real-PTY rendering evidence for representative inline and block math per issue #37, then the T4 ledger flip from `planned` to `landed` recording the rendered-vs-fallback contract.

## Ledger effect

`docs/PARITY_LEDGER.md` T4 evidence now cites this document. T4 status intentionally stays `planned`: this research decides the contract; only PAR-MATH landing the implementation flips the row.

## Non-goals

- No Rust source changes in this ticket; the seams above are directions for PAR-MATH, not edits made here.
- No change to the five-crate topology, the `PARITY_LEDGER.md` workspace contract, or any other ledger row.
- Package-shape parity with upstream's TypeScript exports is not a goal; only the observable rendering behavior is pinned.
