//! Grapheme / visible terminal column width.
//.
//! Semantics match the TypeScript `graphemeWidth` / `visibleWidth` helpers:
//! tab = 3, regional-indicator (incl. singleton) = 2, RGI-style emoji = 2,
//! combining / control / zero-width = 0, East-Asian wide = 2.

use std::sync::{LazyLock, Mutex};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::ansi::extract_ansi_code;

/// ASCII punctuation set used by editor word navigation / break helpers.
pub const PUNCTUATION: &str = "(){}[]<>.,;:'\"!?+-=*/\\|&%^$#@~`";

const WIDTH_CACHE_SIZE: usize = 512;

static WIDTH_CACHE: LazyLock<Mutex<lru_like::LruMap>> =
    LazyLock::new(|| Mutex::new(lru_like::LruMap::new(WIDTH_CACHE_SIZE)));

fn width_cache() -> &'static Mutex<lru_like::LruMap> {
    &WIDTH_CACHE
}

/// Small insertion-order map used only as a bounded width cache.
mod lru_like {
    use std::collections::{HashMap, hash_map::Entry};

    pub(super) struct LruMap {
        map: HashMap<String, usize>,
        order: Vec<String>,
        cap: usize,
    }

    impl LruMap {
        pub(super) fn new(cap: usize) -> Self {
            Self {
                map: HashMap::new(),
                order: Vec::new(),
                cap,
            }
        }

        pub(super) fn get(&self, key: &str) -> Option<usize> {
            self.map.get(key).copied()
        }

        pub(super) fn insert(&mut self, key: String, value: usize) {
            if let Entry::Occupied(mut entry) = self.map.entry(key.clone()) {
                entry.insert(value);
                return;
            }
            if self.order.len() >= self.cap
                && let Some(oldest) = self.order.first().cloned()
            {
                self.order.remove(0);
                self.map.remove(&oldest);
            }
            self.map.insert(key.clone(), value);
            self.order.push(key);
        }
    }
}

// Exact Unicode property range tables (Unicode 16.0).
// Ported from the TypeScript regex properties in utils.ts:
// `terminalSpacingMarkRegex`, `zeroWidthRegex`,
// `leadingNonPrintingRegex`, `nonPrintingCharRegex`, `markCharRegex`.

/// `terminalSpacingMark`: `Spacing_Mark` (Mc) minus U+1734/U+302E/U+302F plus legacy exceptions (190 ranges, Unicode 16.0).
const SPACING_MARK: &[(char, char)] = &[
    ('\u{65F}', '\u{65F}'),
    ('\u{903}', '\u{903}'),
    ('\u{93B}', '\u{93B}'),
    ('\u{93E}', '\u{940}'),
    ('\u{949}', '\u{94C}'),
    ('\u{94E}', '\u{94F}'),
    ('\u{982}', '\u{983}'),
    ('\u{9BE}', '\u{9C0}'),
    ('\u{9C7}', '\u{9C8}'),
    ('\u{9CB}', '\u{9CC}'),
    ('\u{9D7}', '\u{9D7}'),
    ('\u{A03}', '\u{A03}'),
    ('\u{A3E}', '\u{A40}'),
    ('\u{A83}', '\u{A83}'),
    ('\u{ABE}', '\u{AC0}'),
    ('\u{AC9}', '\u{AC9}'),
    ('\u{ACB}', '\u{ACC}'),
    ('\u{B02}', '\u{B03}'),
    ('\u{B3E}', '\u{B3E}'),
    ('\u{B40}', '\u{B40}'),
    ('\u{B47}', '\u{B48}'),
    ('\u{B4B}', '\u{B4C}'),
    ('\u{B57}', '\u{B57}'),
    ('\u{BBE}', '\u{BBF}'),
    ('\u{BC1}', '\u{BC2}'),
    ('\u{BC6}', '\u{BC8}'),
    ('\u{BCA}', '\u{BCC}'),
    ('\u{BD7}', '\u{BD7}'),
    ('\u{C01}', '\u{C03}'),
    ('\u{C41}', '\u{C44}'),
    ('\u{C82}', '\u{C83}'),
    ('\u{CBE}', '\u{CBE}'),
    ('\u{CC0}', '\u{CC4}'),
    ('\u{CC7}', '\u{CC8}'),
    ('\u{CCA}', '\u{CCB}'),
    ('\u{CD5}', '\u{CD6}'),
    ('\u{CF3}', '\u{CF3}'),
    ('\u{D02}', '\u{D03}'),
    ('\u{D3E}', '\u{D40}'),
    ('\u{D46}', '\u{D48}'),
    ('\u{D4A}', '\u{D4C}'),
    ('\u{D57}', '\u{D57}'),
    ('\u{D82}', '\u{D83}'),
    ('\u{DCF}', '\u{DD1}'),
    ('\u{DD8}', '\u{DDF}'),
    ('\u{DF2}', '\u{DF3}'),
    ('\u{F3E}', '\u{F3F}'),
    ('\u{F7F}', '\u{F7F}'),
    ('\u{102B}', '\u{102C}'),
    ('\u{1031}', '\u{1031}'),
    ('\u{1033}', '\u{1035}'),
    ('\u{1038}', '\u{1038}'),
    ('\u{103A}', '\u{103E}'),
    ('\u{1056}', '\u{1057}'),
    ('\u{1062}', '\u{1064}'),
    ('\u{1067}', '\u{106D}'),
    ('\u{1083}', '\u{1084}'),
    ('\u{1087}', '\u{108C}'),
    ('\u{108F}', '\u{108F}'),
    ('\u{109A}', '\u{109C}'),
    ('\u{1715}', '\u{1715}'),
    ('\u{17B6}', '\u{17B6}'),
    ('\u{17BE}', '\u{17C5}'),
    ('\u{17C7}', '\u{17C8}'),
    ('\u{1923}', '\u{1926}'),
    ('\u{1929}', '\u{192B}'),
    ('\u{1930}', '\u{1931}'),
    ('\u{1933}', '\u{1938}'),
    ('\u{1A19}', '\u{1A1A}'),
    ('\u{1A55}', '\u{1A55}'),
    ('\u{1A57}', '\u{1A57}'),
    ('\u{1A61}', '\u{1A61}'),
    ('\u{1A63}', '\u{1A64}'),
    ('\u{1A6D}', '\u{1A72}'),
    ('\u{1B04}', '\u{1B04}'),
    ('\u{1B35}', '\u{1B35}'),
    ('\u{1B3B}', '\u{1B3B}'),
    ('\u{1B3D}', '\u{1B41}'),
    ('\u{1B43}', '\u{1B44}'),
    ('\u{1B82}', '\u{1B82}'),
    ('\u{1BA1}', '\u{1BA1}'),
    ('\u{1BA6}', '\u{1BA7}'),
    ('\u{1BAA}', '\u{1BAA}'),
    ('\u{1BE7}', '\u{1BE7}'),
    ('\u{1BEA}', '\u{1BEC}'),
    ('\u{1BEE}', '\u{1BEE}'),
    ('\u{1BF2}', '\u{1BF3}'),
    ('\u{1C24}', '\u{1C2B}'),
    ('\u{1C34}', '\u{1C35}'),
    ('\u{1CE1}', '\u{1CE1}'),
    ('\u{1CF7}', '\u{1CF7}'),
    ('\u{A823}', '\u{A824}'),
    ('\u{A827}', '\u{A827}'),
    ('\u{A880}', '\u{A881}'),
    ('\u{A8B4}', '\u{A8C3}'),
    ('\u{A952}', '\u{A953}'),
    ('\u{A983}', '\u{A983}'),
    ('\u{A9B4}', '\u{A9B5}'),
    ('\u{A9BA}', '\u{A9BB}'),
    ('\u{A9BE}', '\u{A9C0}'),
    ('\u{AA2F}', '\u{AA30}'),
    ('\u{AA33}', '\u{AA34}'),
    ('\u{AA4D}', '\u{AA4D}'),
    ('\u{AA7B}', '\u{AA7B}'),
    ('\u{AA7D}', '\u{AA7D}'),
    ('\u{AAEB}', '\u{AAEB}'),
    ('\u{AAEE}', '\u{AAEF}'),
    ('\u{AAF5}', '\u{AAF5}'),
    ('\u{ABE3}', '\u{ABE4}'),
    ('\u{ABE6}', '\u{ABE7}'),
    ('\u{ABE9}', '\u{ABEA}'),
    ('\u{ABEC}', '\u{ABEC}'),
    ('\u{11000}', '\u{11000}'),
    ('\u{11002}', '\u{11002}'),
    ('\u{11082}', '\u{11082}'),
    ('\u{110B0}', '\u{110B2}'),
    ('\u{110B7}', '\u{110B8}'),
    ('\u{1112C}', '\u{1112C}'),
    ('\u{11145}', '\u{11146}'),
    ('\u{11182}', '\u{11182}'),
    ('\u{111B3}', '\u{111B5}'),
    ('\u{111BF}', '\u{111C0}'),
    ('\u{111CE}', '\u{111CE}'),
    ('\u{1122C}', '\u{1122E}'),
    ('\u{11232}', '\u{11233}'),
    ('\u{11235}', '\u{11235}'),
    ('\u{112E0}', '\u{112E2}'),
    ('\u{11302}', '\u{11303}'),
    ('\u{1133E}', '\u{1133F}'),
    ('\u{11341}', '\u{11344}'),
    ('\u{11347}', '\u{11348}'),
    ('\u{1134B}', '\u{1134D}'),
    ('\u{11357}', '\u{11357}'),
    ('\u{11362}', '\u{11363}'),
    ('\u{113B8}', '\u{113BA}'),
    ('\u{113C2}', '\u{113C2}'),
    ('\u{113C5}', '\u{113C5}'),
    ('\u{113C7}', '\u{113CA}'),
    ('\u{113CC}', '\u{113CD}'),
    ('\u{113CF}', '\u{113CF}'),
    ('\u{11435}', '\u{11437}'),
    ('\u{11440}', '\u{11441}'),
    ('\u{11445}', '\u{11445}'),
    ('\u{114B0}', '\u{114B2}'),
    ('\u{114B9}', '\u{114B9}'),
    ('\u{114BB}', '\u{114BE}'),
    ('\u{114C1}', '\u{114C1}'),
    ('\u{115AF}', '\u{115B1}'),
    ('\u{115B8}', '\u{115BB}'),
    ('\u{115BE}', '\u{115BE}'),
    ('\u{11630}', '\u{11632}'),
    ('\u{1163B}', '\u{1163C}'),
    ('\u{1163E}', '\u{1163E}'),
    ('\u{116AC}', '\u{116AC}'),
    ('\u{116AE}', '\u{116AF}'),
    ('\u{116B6}', '\u{116B6}'),
    ('\u{1171E}', '\u{1171E}'),
    ('\u{11720}', '\u{11721}'),
    ('\u{11726}', '\u{11726}'),
    ('\u{1182C}', '\u{1182E}'),
    ('\u{11838}', '\u{11838}'),
    ('\u{11930}', '\u{11935}'),
    ('\u{11937}', '\u{11938}'),
    ('\u{1193D}', '\u{1193D}'),
    ('\u{11940}', '\u{11940}'),
    ('\u{11942}', '\u{11942}'),
    ('\u{119D1}', '\u{119D3}'),
    ('\u{119DC}', '\u{119DF}'),
    ('\u{119E4}', '\u{119E4}'),
    ('\u{11A39}', '\u{11A39}'),
    ('\u{11A57}', '\u{11A58}'),
    ('\u{11A97}', '\u{11A97}'),
    ('\u{11C2F}', '\u{11C2F}'),
    ('\u{11C3E}', '\u{11C3E}'),
    ('\u{11CA9}', '\u{11CA9}'),
    ('\u{11CB1}', '\u{11CB1}'),
    ('\u{11CB4}', '\u{11CB4}'),
    ('\u{11D8A}', '\u{11D8E}'),
    ('\u{11D93}', '\u{11D94}'),
    ('\u{11D96}', '\u{11D96}'),
    ('\u{11EF5}', '\u{11EF6}'),
    ('\u{11F03}', '\u{11F03}'),
    ('\u{11F34}', '\u{11F35}'),
    ('\u{11F3E}', '\u{11F3F}'),
    ('\u{11F41}', '\u{11F41}'),
    ('\u{1612A}', '\u{1612C}'),
    ('\u{16F51}', '\u{16F87}'),
    ('\u{16FF0}', '\u{16FF1}'),
    ('\u{1D165}', '\u{1D166}'),
    ('\u{1D16D}', '\u{1D172}'),
];

/// Mark: Mn + Mc + Me (markCharRegex) (321 ranges, Unicode 16.0).
const MARK: &[(char, char)] = &[
    ('\u{300}', '\u{36F}'),
    ('\u{483}', '\u{489}'),
    ('\u{591}', '\u{5BD}'),
    ('\u{5BF}', '\u{5BF}'),
    ('\u{5C1}', '\u{5C2}'),
    ('\u{5C4}', '\u{5C5}'),
    ('\u{5C7}', '\u{5C7}'),
    ('\u{610}', '\u{61A}'),
    ('\u{64B}', '\u{65F}'),
    ('\u{670}', '\u{670}'),
    ('\u{6D6}', '\u{6DC}'),
    ('\u{6DF}', '\u{6E4}'),
    ('\u{6E7}', '\u{6E8}'),
    ('\u{6EA}', '\u{6ED}'),
    ('\u{711}', '\u{711}'),
    ('\u{730}', '\u{74A}'),
    ('\u{7A6}', '\u{7B0}'),
    ('\u{7EB}', '\u{7F3}'),
    ('\u{7FD}', '\u{7FD}'),
    ('\u{816}', '\u{819}'),
    ('\u{81B}', '\u{823}'),
    ('\u{825}', '\u{827}'),
    ('\u{829}', '\u{82D}'),
    ('\u{859}', '\u{85B}'),
    ('\u{897}', '\u{89F}'),
    ('\u{8CA}', '\u{8E1}'),
    ('\u{8E3}', '\u{903}'),
    ('\u{93A}', '\u{93C}'),
    ('\u{93E}', '\u{94F}'),
    ('\u{951}', '\u{957}'),
    ('\u{962}', '\u{963}'),
    ('\u{981}', '\u{983}'),
    ('\u{9BC}', '\u{9BC}'),
    ('\u{9BE}', '\u{9C4}'),
    ('\u{9C7}', '\u{9C8}'),
    ('\u{9CB}', '\u{9CD}'),
    ('\u{9D7}', '\u{9D7}'),
    ('\u{9E2}', '\u{9E3}'),
    ('\u{9FE}', '\u{9FE}'),
    ('\u{A01}', '\u{A03}'),
    ('\u{A3C}', '\u{A3C}'),
    ('\u{A3E}', '\u{A42}'),
    ('\u{A47}', '\u{A48}'),
    ('\u{A4B}', '\u{A4D}'),
    ('\u{A51}', '\u{A51}'),
    ('\u{A70}', '\u{A71}'),
    ('\u{A75}', '\u{A75}'),
    ('\u{A81}', '\u{A83}'),
    ('\u{ABC}', '\u{ABC}'),
    ('\u{ABE}', '\u{AC5}'),
    ('\u{AC7}', '\u{AC9}'),
    ('\u{ACB}', '\u{ACD}'),
    ('\u{AE2}', '\u{AE3}'),
    ('\u{AFA}', '\u{AFF}'),
    ('\u{B01}', '\u{B03}'),
    ('\u{B3C}', '\u{B3C}'),
    ('\u{B3E}', '\u{B44}'),
    ('\u{B47}', '\u{B48}'),
    ('\u{B4B}', '\u{B4D}'),
    ('\u{B55}', '\u{B57}'),
    ('\u{B62}', '\u{B63}'),
    ('\u{B82}', '\u{B82}'),
    ('\u{BBE}', '\u{BC2}'),
    ('\u{BC6}', '\u{BC8}'),
    ('\u{BCA}', '\u{BCD}'),
    ('\u{BD7}', '\u{BD7}'),
    ('\u{C00}', '\u{C04}'),
    ('\u{C3C}', '\u{C3C}'),
    ('\u{C3E}', '\u{C44}'),
    ('\u{C46}', '\u{C48}'),
    ('\u{C4A}', '\u{C4D}'),
    ('\u{C55}', '\u{C56}'),
    ('\u{C62}', '\u{C63}'),
    ('\u{C81}', '\u{C83}'),
    ('\u{CBC}', '\u{CBC}'),
    ('\u{CBE}', '\u{CC4}'),
    ('\u{CC6}', '\u{CC8}'),
    ('\u{CCA}', '\u{CCD}'),
    ('\u{CD5}', '\u{CD6}'),
    ('\u{CE2}', '\u{CE3}'),
    ('\u{CF3}', '\u{CF3}'),
    ('\u{D00}', '\u{D03}'),
    ('\u{D3B}', '\u{D3C}'),
    ('\u{D3E}', '\u{D44}'),
    ('\u{D46}', '\u{D48}'),
    ('\u{D4A}', '\u{D4D}'),
    ('\u{D57}', '\u{D57}'),
    ('\u{D62}', '\u{D63}'),
    ('\u{D81}', '\u{D83}'),
    ('\u{DCA}', '\u{DCA}'),
    ('\u{DCF}', '\u{DD4}'),
    ('\u{DD6}', '\u{DD6}'),
    ('\u{DD8}', '\u{DDF}'),
    ('\u{DF2}', '\u{DF3}'),
    ('\u{E31}', '\u{E31}'),
    ('\u{E34}', '\u{E3A}'),
    ('\u{E47}', '\u{E4E}'),
    ('\u{EB1}', '\u{EB1}'),
    ('\u{EB4}', '\u{EBC}'),
    ('\u{EC8}', '\u{ECE}'),
    ('\u{F18}', '\u{F19}'),
    ('\u{F35}', '\u{F35}'),
    ('\u{F37}', '\u{F37}'),
    ('\u{F39}', '\u{F39}'),
    ('\u{F3E}', '\u{F3F}'),
    ('\u{F71}', '\u{F84}'),
    ('\u{F86}', '\u{F87}'),
    ('\u{F8D}', '\u{F97}'),
    ('\u{F99}', '\u{FBC}'),
    ('\u{FC6}', '\u{FC6}'),
    ('\u{102B}', '\u{103E}'),
    ('\u{1056}', '\u{1059}'),
    ('\u{105E}', '\u{1060}'),
    ('\u{1062}', '\u{1064}'),
    ('\u{1067}', '\u{106D}'),
    ('\u{1071}', '\u{1074}'),
    ('\u{1082}', '\u{108D}'),
    ('\u{108F}', '\u{108F}'),
    ('\u{109A}', '\u{109D}'),
    ('\u{135D}', '\u{135F}'),
    ('\u{1712}', '\u{1715}'),
    ('\u{1732}', '\u{1734}'),
    ('\u{1752}', '\u{1753}'),
    ('\u{1772}', '\u{1773}'),
    ('\u{17B4}', '\u{17D3}'),
    ('\u{17DD}', '\u{17DD}'),
    ('\u{180B}', '\u{180D}'),
    ('\u{180F}', '\u{180F}'),
    ('\u{1885}', '\u{1886}'),
    ('\u{18A9}', '\u{18A9}'),
    ('\u{1920}', '\u{192B}'),
    ('\u{1930}', '\u{193B}'),
    ('\u{1A17}', '\u{1A1B}'),
    ('\u{1A55}', '\u{1A5E}'),
    ('\u{1A60}', '\u{1A7C}'),
    ('\u{1A7F}', '\u{1A7F}'),
    ('\u{1AB0}', '\u{1ACE}'),
    ('\u{1B00}', '\u{1B04}'),
    ('\u{1B34}', '\u{1B44}'),
    ('\u{1B6B}', '\u{1B73}'),
    ('\u{1B80}', '\u{1B82}'),
    ('\u{1BA1}', '\u{1BAD}'),
    ('\u{1BE6}', '\u{1BF3}'),
    ('\u{1C24}', '\u{1C37}'),
    ('\u{1CD0}', '\u{1CD2}'),
    ('\u{1CD4}', '\u{1CE8}'),
    ('\u{1CED}', '\u{1CED}'),
    ('\u{1CF4}', '\u{1CF4}'),
    ('\u{1CF7}', '\u{1CF9}'),
    ('\u{1DC0}', '\u{1DFF}'),
    ('\u{20D0}', '\u{20F0}'),
    ('\u{2CEF}', '\u{2CF1}'),
    ('\u{2D7F}', '\u{2D7F}'),
    ('\u{2DE0}', '\u{2DFF}'),
    ('\u{302A}', '\u{302F}'),
    ('\u{3099}', '\u{309A}'),
    ('\u{A66F}', '\u{A672}'),
    ('\u{A674}', '\u{A67D}'),
    ('\u{A69E}', '\u{A69F}'),
    ('\u{A6F0}', '\u{A6F1}'),
    ('\u{A802}', '\u{A802}'),
    ('\u{A806}', '\u{A806}'),
    ('\u{A80B}', '\u{A80B}'),
    ('\u{A823}', '\u{A827}'),
    ('\u{A82C}', '\u{A82C}'),
    ('\u{A880}', '\u{A881}'),
    ('\u{A8B4}', '\u{A8C5}'),
    ('\u{A8E0}', '\u{A8F1}'),
    ('\u{A8FF}', '\u{A8FF}'),
    ('\u{A926}', '\u{A92D}'),
    ('\u{A947}', '\u{A953}'),
    ('\u{A980}', '\u{A983}'),
    ('\u{A9B3}', '\u{A9C0}'),
    ('\u{A9E5}', '\u{A9E5}'),
    ('\u{AA29}', '\u{AA36}'),
    ('\u{AA43}', '\u{AA43}'),
    ('\u{AA4C}', '\u{AA4D}'),
    ('\u{AA7B}', '\u{AA7D}'),
    ('\u{AAB0}', '\u{AAB0}'),
    ('\u{AAB2}', '\u{AAB4}'),
    ('\u{AAB7}', '\u{AAB8}'),
    ('\u{AABE}', '\u{AABF}'),
    ('\u{AAC1}', '\u{AAC1}'),
    ('\u{AAEB}', '\u{AAEF}'),
    ('\u{AAF5}', '\u{AAF6}'),
    ('\u{ABE3}', '\u{ABEA}'),
    ('\u{ABEC}', '\u{ABED}'),
    ('\u{FB1E}', '\u{FB1E}'),
    ('\u{FE00}', '\u{FE0F}'),
    ('\u{FE20}', '\u{FE2F}'),
    ('\u{101FD}', '\u{101FD}'),
    ('\u{102E0}', '\u{102E0}'),
    ('\u{10376}', '\u{1037A}'),
    ('\u{10A01}', '\u{10A03}'),
    ('\u{10A05}', '\u{10A06}'),
    ('\u{10A0C}', '\u{10A0F}'),
    ('\u{10A38}', '\u{10A3A}'),
    ('\u{10A3F}', '\u{10A3F}'),
    ('\u{10AE5}', '\u{10AE6}'),
    ('\u{10D24}', '\u{10D27}'),
    ('\u{10D69}', '\u{10D6D}'),
    ('\u{10EAB}', '\u{10EAC}'),
    ('\u{10EFC}', '\u{10EFF}'),
    ('\u{10F46}', '\u{10F50}'),
    ('\u{10F82}', '\u{10F85}'),
    ('\u{11000}', '\u{11002}'),
    ('\u{11038}', '\u{11046}'),
    ('\u{11070}', '\u{11070}'),
    ('\u{11073}', '\u{11074}'),
    ('\u{1107F}', '\u{11082}'),
    ('\u{110B0}', '\u{110BA}'),
    ('\u{110C2}', '\u{110C2}'),
    ('\u{11100}', '\u{11102}'),
    ('\u{11127}', '\u{11134}'),
    ('\u{11145}', '\u{11146}'),
    ('\u{11173}', '\u{11173}'),
    ('\u{11180}', '\u{11182}'),
    ('\u{111B3}', '\u{111C0}'),
    ('\u{111C9}', '\u{111CC}'),
    ('\u{111CE}', '\u{111CF}'),
    ('\u{1122C}', '\u{11237}'),
    ('\u{1123E}', '\u{1123E}'),
    ('\u{11241}', '\u{11241}'),
    ('\u{112DF}', '\u{112EA}'),
    ('\u{11300}', '\u{11303}'),
    ('\u{1133B}', '\u{1133C}'),
    ('\u{1133E}', '\u{11344}'),
    ('\u{11347}', '\u{11348}'),
    ('\u{1134B}', '\u{1134D}'),
    ('\u{11357}', '\u{11357}'),
    ('\u{11362}', '\u{11363}'),
    ('\u{11366}', '\u{1136C}'),
    ('\u{11370}', '\u{11374}'),
    ('\u{113B8}', '\u{113C0}'),
    ('\u{113C2}', '\u{113C2}'),
    ('\u{113C5}', '\u{113C5}'),
    ('\u{113C7}', '\u{113CA}'),
    ('\u{113CC}', '\u{113D0}'),
    ('\u{113D2}', '\u{113D2}'),
    ('\u{113E1}', '\u{113E2}'),
    ('\u{11435}', '\u{11446}'),
    ('\u{1145E}', '\u{1145E}'),
    ('\u{114B0}', '\u{114C3}'),
    ('\u{115AF}', '\u{115B5}'),
    ('\u{115B8}', '\u{115C0}'),
    ('\u{115DC}', '\u{115DD}'),
    ('\u{11630}', '\u{11640}'),
    ('\u{116AB}', '\u{116B7}'),
    ('\u{1171D}', '\u{1172B}'),
    ('\u{1182C}', '\u{1183A}'),
    ('\u{11930}', '\u{11935}'),
    ('\u{11937}', '\u{11938}'),
    ('\u{1193B}', '\u{1193E}'),
    ('\u{11940}', '\u{11940}'),
    ('\u{11942}', '\u{11943}'),
    ('\u{119D1}', '\u{119D7}'),
    ('\u{119DA}', '\u{119E0}'),
    ('\u{119E4}', '\u{119E4}'),
    ('\u{11A01}', '\u{11A0A}'),
    ('\u{11A33}', '\u{11A39}'),
    ('\u{11A3B}', '\u{11A3E}'),
    ('\u{11A47}', '\u{11A47}'),
    ('\u{11A51}', '\u{11A5B}'),
    ('\u{11A8A}', '\u{11A99}'),
    ('\u{11C2F}', '\u{11C36}'),
    ('\u{11C38}', '\u{11C3F}'),
    ('\u{11C92}', '\u{11CA7}'),
    ('\u{11CA9}', '\u{11CB6}'),
    ('\u{11D31}', '\u{11D36}'),
    ('\u{11D3A}', '\u{11D3A}'),
    ('\u{11D3C}', '\u{11D3D}'),
    ('\u{11D3F}', '\u{11D45}'),
    ('\u{11D47}', '\u{11D47}'),
    ('\u{11D8A}', '\u{11D8E}'),
    ('\u{11D90}', '\u{11D91}'),
    ('\u{11D93}', '\u{11D97}'),
    ('\u{11EF3}', '\u{11EF6}'),
    ('\u{11F00}', '\u{11F01}'),
    ('\u{11F03}', '\u{11F03}'),
    ('\u{11F34}', '\u{11F3A}'),
    ('\u{11F3E}', '\u{11F42}'),
    ('\u{11F5A}', '\u{11F5A}'),
    ('\u{13440}', '\u{13440}'),
    ('\u{13447}', '\u{13455}'),
    ('\u{1611E}', '\u{1612F}'),
    ('\u{16AF0}', '\u{16AF4}'),
    ('\u{16B30}', '\u{16B36}'),
    ('\u{16F4F}', '\u{16F4F}'),
    ('\u{16F51}', '\u{16F87}'),
    ('\u{16F8F}', '\u{16F92}'),
    ('\u{16FE4}', '\u{16FE4}'),
    ('\u{16FF0}', '\u{16FF1}'),
    ('\u{1BC9D}', '\u{1BC9E}'),
    ('\u{1CF00}', '\u{1CF2D}'),
    ('\u{1CF30}', '\u{1CF46}'),
    ('\u{1D165}', '\u{1D169}'),
    ('\u{1D16D}', '\u{1D172}'),
    ('\u{1D17B}', '\u{1D182}'),
    ('\u{1D185}', '\u{1D18B}'),
    ('\u{1D1AA}', '\u{1D1AD}'),
    ('\u{1D242}', '\u{1D244}'),
    ('\u{1DA00}', '\u{1DA36}'),
    ('\u{1DA3B}', '\u{1DA6C}'),
    ('\u{1DA75}', '\u{1DA75}'),
    ('\u{1DA84}', '\u{1DA84}'),
    ('\u{1DA9B}', '\u{1DA9F}'),
    ('\u{1DAA1}', '\u{1DAAF}'),
    ('\u{1E000}', '\u{1E006}'),
    ('\u{1E008}', '\u{1E018}'),
    ('\u{1E01B}', '\u{1E021}'),
    ('\u{1E023}', '\u{1E024}'),
    ('\u{1E026}', '\u{1E02A}'),
    ('\u{1E08F}', '\u{1E08F}'),
    ('\u{1E130}', '\u{1E136}'),
    ('\u{1E2AE}', '\u{1E2AE}'),
    ('\u{1E2EC}', '\u{1E2EF}'),
    ('\u{1E4EC}', '\u{1E4EF}'),
    ('\u{1E5EE}', '\u{1E5EF}'),
    ('\u{1E8D0}', '\u{1E8D6}'),
    ('\u{1E944}', '\u{1E94A}'),
    ('\u{E0100}', '\u{E01EF}'),
];

/// Control: Cc (2 ranges, Unicode 16.0).
const CONTROL: &[(char, char)] = &[('\u{0}', '\u{1F}'), ('\u{7F}', '\u{9F}')];

/// Format: Cf (21 ranges, Unicode 16.0).
const FORMAT: &[(char, char)] = &[
    ('\u{AD}', '\u{AD}'),
    ('\u{600}', '\u{605}'),
    ('\u{61C}', '\u{61C}'),
    ('\u{6DD}', '\u{6DD}'),
    ('\u{70F}', '\u{70F}'),
    ('\u{890}', '\u{891}'),
    ('\u{8E2}', '\u{8E2}'),
    ('\u{180E}', '\u{180E}'),
    ('\u{200B}', '\u{200F}'),
    ('\u{202A}', '\u{202E}'),
    ('\u{2060}', '\u{2064}'),
    ('\u{2066}', '\u{206F}'),
    ('\u{FEFF}', '\u{FEFF}'),
    ('\u{FFF9}', '\u{FFFB}'),
    ('\u{110BD}', '\u{110BD}'),
    ('\u{110CD}', '\u{110CD}'),
    ('\u{13430}', '\u{1343F}'),
    ('\u{1BCA0}', '\u{1BCA3}'),
    ('\u{1D173}', '\u{1D17A}'),
    ('\u{E0001}', '\u{E0001}'),
    ('\u{E0020}', '\u{E007F}'),
];

/// `Default_Ignorable_Code_Point` (19 ranges, Unicode 16.0).
const DICP: &[(char, char)] = &[
    ('\u{AD}', '\u{AD}'),
    ('\u{34F}', '\u{34F}'),
    ('\u{61C}', '\u{61C}'),
    ('\u{115F}', '\u{1160}'),
    ('\u{17B4}', '\u{17B5}'),
    ('\u{180B}', '\u{180F}'),
    ('\u{200B}', '\u{200F}'),
    ('\u{202A}', '\u{202E}'),
    ('\u{2060}', '\u{2064}'),
    ('\u{2065}', '\u{2065}'),
    ('\u{2066}', '\u{206F}'),
    ('\u{3164}', '\u{3164}'),
    ('\u{FE00}', '\u{FE0F}'),
    ('\u{FEFF}', '\u{FEFF}'),
    ('\u{FFA0}', '\u{FFA0}'),
    ('\u{FFF0}', '\u{FFF8}'),
    ('\u{1BCA0}', '\u{1BCA3}'),
    ('\u{1D173}', '\u{1D17A}'),
    ('\u{E0000}', '\u{E0FFF}'),
];

/// Surrogate: Cs (U+D800..U+DFFF).  Rust `char` cannot represent surrogates,
/// so we store the range as `u32` and check the code point directly.
const SURROGATE_RANGE: (u32, u32) = (0xD800, 0xDFFF);

/// Binary-search a sorted `(char, char)` range table for `c`.
fn char_in_ranges(c: char, ranges: &[(char, char)]) -> bool {
    ranges
        .binary_search_by(|&(start, end)| {
            if end < c {
                core::cmp::Ordering::Less
            } else if start > c {
                core::cmp::Ordering::Greater
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `true` when `c` is a terminal spacing mark (Mc minus exclusions plus exceptions).
fn is_terminal_spacing_mark_char(c: char) -> bool {
    char_in_ranges(c, SPACING_MARK)
}

/// `true` when `c` has the Unicode `Mark` property (Mn + Mc + Me).
fn is_mark_char(c: char) -> bool {
    char_in_ranges(c, MARK)
}

/// `true` when `c` is a `Default_Ignorable_Code_Point`.
fn is_default_ignorable(c: char) -> bool {
    char_in_ranges(c, DICP)
}

/// `true` when `c` is a Control character (Cc).
fn is_control_char(c: char) -> bool {
    char_in_ranges(c, CONTROL)
}

/// `true` when `c` is a Format character (Cf).
fn is_format_char(c: char) -> bool {
    char_in_ranges(c, FORMAT)
}

/// `true` when `c` is a Surrogate (Cs).  Rust `char` excludes surrogates,
/// so this checks the code point against the surrogate range.
fn is_surrogate(c: char) -> bool {
    let cp = c as u32;
    (SURROGATE_RANGE.0..=SURROGATE_RANGE.1).contains(&cp)
}

/// `true` when `c` is non-printing: DICP, Control, Format, Mark, or Surrogate.
/// Mirrors `nonPrintingCharRegex` / `leadingNonPrintingRegex` in utils.ts.
fn is_non_printing_char(c: char) -> bool {
    is_default_ignorable(c)
        || is_control_char(c)
        || is_format_char(c)
        || is_mark_char(c)
        || is_surrogate(c)
}

/// `true` when every char in `segment` is zero-width per `zeroWidthRegex`:
/// DICP, Control, Mark, or Surrogate (note: Format is NOT included).
fn is_zero_width_cluster(segment: &str) -> bool {
    !segment.is_empty()
        && segment.chars().all(|c| {
            is_default_ignorable(c) || is_control_char(c) || is_mark_char(c) || is_surrogate(c)
        })
}

/// Strip leading non-printing chars (DICP, Control, Format, Mark, Surrogate).
/// Mirrors `leadingNonPrintingRegex` in utils.ts.
fn strip_leading_non_printing(segment: &str) -> &str {
    let byte_index = segment
        .char_indices()
        .find_map(|(index, ch)| (!is_non_printing_char(ch)).then_some(index))
        .unwrap_or(segment.len());
    &segment[byte_index..]
}
/// Return `true` when every character is printable ASCII (`0x20..=0x7E`).
#[must_use]
pub fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// East-Asian / terminal cell width of a single code point (non-CJK ambiguous = 1).
#[must_use]
pub fn east_asian_width(c: char) -> usize {
    c.width().unwrap_or(0)
}
/// Fast pre-filter mirroring the TS `couldBeEmoji` heuristic.
#[must_use]
pub fn could_be_emoji(segment: &str) -> bool {
    let Some(cp) = segment.chars().next().map(|c| c as u32) else {
        return false;
    };
    (0x1f_000..=0x1f_bff).contains(&cp)
        || (0x2300..=0x23ff).contains(&cp)
        || (0x2600..=0x27bf).contains(&cp)
        || (0x2b50..=0x2b55).contains(&cp)
        || segment.contains('\u{FE0F}')
        || segment.chars().count() > 2
}

/// Approximate RGI emoji detection using `unicode-width` sequence rules.
/// Mirrors `rgiEmojiRegex.test(segment)` from utils.ts: only true when the
/// segment is an actual emoji-width sequence, not merely one that *contains*
/// a VS16 or ZWJ (e.g. "B\u{FE0F}" is not RGI emoji).
fn is_rgi_emojiish(segment: &str) -> bool {
    if segment.width() == 2 && could_be_emoji(segment) {
        return true;
    }
    // ZWJ / skin-tone sequences that some tables report as width 1 still
    // render as emoji cells in terminals pi targets.  Only match when the
    // base code point is in an emoji block.
    if let Some(base) = segment.chars().next() {
        let cp = base as u32;
        let base_is_emoji = (0x1f_000..=0x1f_bff).contains(&cp)
            || (0x2600..=0x27bf).contains(&cp)
            || (0x2b50..=0x2b55).contains(&cp)
            || (0x2300..=0x23ff).contains(&cp);
        if base_is_emoji
            && (segment.contains('\u{200D}')
                || segment.contains('\u{FE0F}')
                || segment
                    .chars()
                    .any(|c| (0x1f_3fb..=0x1f_3ff).contains(&(c as u32))))
        {
            return true;
        }
    }
    false
}

/// Terminal width of a single grapheme cluster.
///
/// Ports `graphemeWidth` from utils.ts exactly:
/// 1. tab → 3
/// 2. terminal spacing marks (entire cluster) → code-point count
/// 3. zero-width clusters (DICP ∪ Control ∪ Mark ∪ Surrogate) → 0
/// 4. RGI emoji → 2
/// 5. base width + trailing visible chars (spacing marks +1, mark-followed
///    consonants, halfwidth/fullwidth forms, Thai/Lao AM vowels)
#[must_use]
pub fn grapheme_width(segment: &str) -> usize {
    if segment == "\t" {
        return 3;
    }

    // Some marks occupy cells even without a base character.
    if !segment.is_empty() && segment.chars().all(is_terminal_spacing_mark_char) {
        return segment.chars().count();
    }

    // Zero-width clusters (DICP, Control, Mark, Surrogate — not Format).
    if is_zero_width_cluster(segment) {
        return 0;
    }

    // Emoji check with pre-filter.
    if could_be_emoji(segment) && is_rgi_emojiish(segment) {
        return 2;
    }

    // Get base visible codepoint.
    let base = strip_leading_non_printing(segment);
    let Some(cp) = base.chars().next() else {
        return 0;
    };
    let cp_u = cp as u32;

    // Regional indicator symbols are rendered as full-width emoji.
    if (0x1f_1e6..=0x1f_1ff).contains(&cp_u) {
        return 2;
    }

    let mut width = east_asian_width(cp);

    // Count trailing visible code points that terminals may allocate cells for:
    // spacing marks, Indic consonants after marks, halfwidth/fullwidth forms,
    // and Thai/Lao AM vowels.
    if base.chars().count() > 1 {
        let mut follows_mark = false;
        for c in base.chars().skip(1) {
            let cu = c as u32;
            if is_terminal_spacing_mark_char(c) {
                width += 1;
                follows_mark = false;
            } else if is_mark_char(c) {
                follows_mark = true;
            } else if !is_non_printing_char(c) {
                if follows_mark || (0xff00..=0xffef).contains(&cu) {
                    width = width.saturating_add(east_asian_width(c));
                } else if cu == 0x0e33 || cu == 0x0eb3 {
                    width = width.saturating_add(1);
                }
                follows_mark = false;
            }
        }
    }

    width
}
/// Visible terminal columns of `str`, ignoring ANSI/OSC/APC and counting tabs as 3.
#[must_use]
pub fn visible_width(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    if is_printable_ascii(s) {
        return s.len();
    }
    if let Ok(cache) = width_cache().lock()
        && let Some(width) = cache.get(s)
    {
        return width;
    }

    let mut clean = s.to_owned();
    if clean.contains('\t') {
        clean = clean.replace('\t', "   ");
    }
    if clean.contains('\u{1b}') {
        let mut stripped = String::with_capacity(clean.len());
        let mut i = 0;
        while i < clean.len() {
            if let Some(ansi) = extract_ansi_code(&clean, i) {
                i += ansi.len;
                continue;
            }
            // Copy one UTF-8 char.
            let ch = clean[i..].chars().next().map_or(1, char::len_utf8);
            // SAFETY: i is a char boundary by construction.
            stripped.push_str(&clean[i..i + ch]);
            i += ch;
        }
        clean = stripped;
    }

    let mut width = 0usize;
    for grapheme in clean.graphemes(true) {
        width = width.saturating_add(grapheme_width(grapheme));
    }

    if let Ok(mut cache) = width_cache().lock() {
        cache.insert(s.to_owned(), width);
    }
    width
}

/// Normalize text for terminal output without changing logical editor content.
///
/// Expands standalone tabs outside escapes to three spaces and splits Thai/Lao
/// AM vowels so terminal cells match editor width accounting.
#[must_use]
pub fn normalize_terminal_output(s: &str) -> String {
    let mut normalized = s.to_owned();
    if normalized.contains('\u{0e33}') || normalized.contains('\u{0eb3}') {
        let mut out = String::with_capacity(normalized.len() + 8);
        for ch in normalized.chars() {
            match ch {
                '\u{0e33}' => out.push_str("\u{0e4d}\u{0e32}"),
                '\u{0eb3}' => out.push_str("\u{0ecd}\u{0eb2}"),
                other => out.push(other),
            }
        }
        normalized = out;
    }
    if !normalized.contains('\t') {
        return normalized;
    }

    let mut result = String::with_capacity(normalized.len());
    let mut i = 0;
    while i < normalized.len() {
        if let Some(ansi) = extract_ansi_code(&normalized, i) {
            result.push_str(ansi.code);
            i += ansi.len;
            continue;
        }
        let ch = normalized[i..].chars().next().unwrap_or('\0');
        if ch == '\t' {
            result.push_str("   ");
            i += 1;
        } else {
            let len = ch.len_utf8();
            result.push_str(&normalized[i..i + len]);
            i += len;
        }
    }
    result
}

/// `true` when the grapheme is a CJK break opportunity (Han/Hira/Kata/Hangul/Bopomofo).
#[must_use]
pub fn cjk_break_grapheme(segment: &str) -> bool {
    let Some(c) = segment.chars().next() else {
        return false;
    };
    matches!(
        c,
        // Bopomofo
        '\u{3100}'..='\u{312F}'
            | '\u{31A0}'..='\u{31BF}'
            // Hiragana / Katakana
            | '\u{3040}'..='\u{309F}'
            | '\u{30A0}'..='\u{30FF}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{FF65}'..='\u{FF9F}'
            // Hangul
            | '\u{1100}'..='\u{11FF}'
            | '\u{3130}'..='\u{318F}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7AF}'
            | '\u{D7B0}'..='\u{D7FF}'
            // Han / CJK
            | '\u{2E80}'..='\u{2EFF}'
            | '\u{2F00}'..='\u{2FDF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2A6DF}'
            | '\u{2A700}'..='\u{2B73F}'
            | '\u{2B740}'..='\u{2B81F}'
            | '\u{2B820}'..='\u{2CEAF}'
            | '\u{2CEB0}'..='\u{2EBEF}'
            | '\u{30000}'..='\u{3134F}'
            | '\u{31350}'..='\u{323AF}'
    )
}

/// Whitespace character (Unicode `White_Space`).
#[must_use]
pub fn is_whitespace_char(ch: &str) -> bool {
    ch.chars().next().is_some_and(char::is_whitespace) && ch.chars().count() == 1
}

/// Punctuation character from the editor punctuation set.
#[must_use]
pub fn is_punctuation_char(ch: &str) -> bool {
    ch.chars().count() == 1 && ch.chars().next().is_some_and(|c| PUNCTUATION.contains(c))
}

/// Apply a background painter to a line after padding it to `width`.
///
/// `bg_fn` receives the already-padded content and returns styled text.
#[must_use]
pub fn apply_background_to_line(
    line: &str,
    width: usize,
    bg_fn: impl FnOnce(&str) -> String,
) -> String {
    let visible_len = visible_width(line);
    let padding_needed = width.saturating_sub(visible_len);
    let with_padding = format!("{line}{}", " ".repeat(padding_needed));
    bg_fn(&with_padding)
}
