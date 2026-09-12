//! Thin wasm-bindgen surface — what the JS workers see.
//!
//! `scan_chunk` is the hot path. `inspect_seed` is the "Verify a seed" path
//! used by the Balatropedia UI to show *where* each filter clause matches
//! on a single seed, without re-running the brute force.

use wasm_bindgen::prelude::*;
use crate::filter::{Clause, Filter};
use crate::search::Searcher;
use crate::state::{Deck, Stake, RunConfig};
use crate::instance::Instance;
use crate::derive::{
    next_boss, next_pack, next_tag, next_voucher, next_shop_item,
    open_pack, open_pack_detailed, resolve_soul_legendary, resolve_wraith_rare,
    Edition, PackContents, ShopItemType,Rarity,
};

#[wasm_bindgen]
pub fn init() {
    #[cfg(feature = "wasm")]
    console_error_panic_hook::set_once();
}

/// Scan a chunk and return packed match records.
/// Each record: 8 bytes rank LE + 1 byte score + 8 bytes seed (right-padded).
/// = 17 bytes per match.
#[wasm_bindgen]
pub fn scan_chunk(
    filter_json: &str,
    start_rank: u64,
    count: u64,
    seed_len: u8,
    deck_idx: u8,
    stake_idx: u8,
    partial: bool,
    min_score: u8,
) -> Vec<u8> {
    let filter: Filter = serde_json::from_str(filter_json).unwrap_or_default();
    let compiled = filter.compile();

    let config = RunConfig {
        deck: deck_from_idx(deck_idx),
        stake: stake_from_idx(stake_idx),
        seed: [0; 8],
        seed_len,
    };

    let s = Searcher {
        config: &config,
        filter: &compiled,
        partial,
        min_score,
    };

    let mut out: Vec<u8> = Vec::with_capacity(64 * 17);
    s.scan(start_rank, count, |m| {
        out.extend_from_slice(&m.rank.to_le_bytes());
        out.push(m.score);
        let mut buf = [b' '; 8];
        let b = m.seed.as_bytes();
        buf[..b.len()].copy_from_slice(b);
        out.extend_from_slice(&buf);
    });
    out
}

/// Parallel scan over `[start_rank, start_rank + count)`. Same output
/// format as `scan_chunk`. Requires the host page to be cross-origin
/// isolated (COOP/COEP) so `SharedArrayBuffer` is available; the JS
/// caller must have invoked `initThreadPool(threadCount)` first.
///
/// Output is rank-ordered identically to the serial version. Determinism
/// is preserved because each seed's evaluation is a pure function of the
/// seed string + config.
#[cfg(feature = "wasm-threads")]
#[wasm_bindgen]
pub fn scan_chunk_parallel(
    filter_json: &str,
    start_rank: u64,
    count: u64,
    seed_len: u8,
    deck_idx: u8,
    stake_idx: u8,
    partial: bool,
    min_score: u8,
) -> Vec<u8> {
    let filter: Filter = serde_json::from_str(filter_json).unwrap_or_default();
    let compiled = filter.compile();

    let config = RunConfig {
        deck: deck_from_idx(deck_idx),
        stake: stake_from_idx(stake_idx),
        seed: [0; 8],
        seed_len,
    };

    let s = Searcher {
        config: &config,
        filter: &compiled,
        partial,
        min_score,
    };

    let matches = s.scan_parallel(start_rank, count);
    let mut out: Vec<u8> = Vec::with_capacity(matches.len() * 17);
    for m in &matches {
        out.extend_from_slice(&m.rank.to_le_bytes());
        out.push(m.score);
        let mut buf = [b' '; 8];
        let b = m.seed.as_bytes();
        buf[..b.len()].copy_from_slice(b);
        out.extend_from_slice(&buf);
    }
    out
}

/// Re-export of `wasm_bindgen_rayon::init_thread_pool` so the JS host can
/// call `import('balatro_seed_engine').initThreadPool(navigator.hardwareConcurrency)`
/// before any `scan_chunk_parallel` invocation. Returns a promise that
/// resolves once all worker threads have signalled ready.
#[cfg(feature = "wasm-threads")]
pub use wasm_bindgen_rayon::init_thread_pool;

/// Inspect a single seed: run each filter clause and return a JSON report
/// describing which clauses matched and where. Used by the "Verify a seed"
/// UI panel.
#[wasm_bindgen]
pub fn inspect_seed(
    filter_json: &str,
    seed: &str,
    deck_idx: u8,
    stake_idx: u8,
) -> String {
    let filter: Filter = match serde_json::from_str(filter_json) {
        Ok(f) => f,
        Err(e) => return format!("{{\"ok\":false,\"error\":\"{}\"}}", json_escape(&e.to_string())),
    };

    let mut out = String::with_capacity(512);
    out.push_str("{\"ok\":true,\"seed\":\"");
    out.push_str(&json_escape(seed));
    out.push_str("\",\"clauses\":[");
    let mut matched = 0u32;
    for (idx, clause) in filter.clauses.iter().enumerate() {
        if idx > 0 { out.push(','); }
        let (hit, detail) = inspect_clause(clause, seed, deck_idx, stake_idx);
        if hit { matched += 1; }
        out.push_str("{\"index\":");
        out.push_str(&idx.to_string());
        out.push_str(",\"matched\":");
        out.push_str(if hit { "true" } else { "false" });
        out.push_str(",\"detail\":\"");
        out.push_str(&json_escape(&detail));
        out.push_str("\"}");
    }
    out.push_str("],\"matched\":");
    out.push_str(&matched.to_string());
    out.push_str(",\"total\":");
    out.push_str(&filter.clauses.len().to_string());
    out.push('}');
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
#[wasm_bindgen]
pub fn analyze_seed_basic(
    seed: &str,
    max_ante: u8,
    deck_idx: u8,
    stake_idx: u8,
) -> String {
    let max_ante = max_ante.clamp(1, 16);

    let mut out = String::with_capacity(8192);

    out.push_str("{\"ok\":true,\"seed\":\"");
    out.push_str(&json_escape(seed));
    out.push_str("\",\"antes\":[");

    for ante in 1..=max_ante {
        if ante > 1 {
            out.push(',');
        }

        let ante_i32 = ante as i32;

        // Boss
        let mut boss_inst =
            fresh_instance(seed, deck_idx, stake_idx);
        let boss =
            next_boss(&mut boss_inst, ante_i32);

        // Voucher
        let mut voucher_inst =
            fresh_instance(seed, deck_idx, stake_idx);
        let voucher =
            next_voucher(&mut voucher_inst, ante_i32);

        // Tags
        let mut tag_inst =
            fresh_instance(seed, deck_idx, stake_idx);
        let small_tag =
            next_tag(&mut tag_inst, ante_i32);
        let big_tag =
            next_tag(&mut tag_inst, ante_i32);

        // Shop - 첫 6칸
        let mut shop_inst =
    fresh_instance(seed, deck_idx, stake_idx);
        let shop1 = next_shop_item(&mut shop_inst, ante_i32);
let shop2 = next_shop_item(&mut shop_inst, ante_i32);
let shop3 = next_shop_item(&mut shop_inst, ante_i32);
let shop4 = next_shop_item(&mut shop_inst, ante_i32);
let shop5 = next_shop_item(&mut shop_inst, ante_i32);
let shop6 = next_shop_item(&mut shop_inst, ante_i32);
            
        // Booster packs - 첫 2개
        let mut pack_inst =
            fresh_instance(seed, deck_idx, stake_idx);

        let pack1 =
            next_pack(&mut pack_inst, ante_i32);
        let pack1_contents =
            open_pack_detailed(
                &mut pack_inst,
                pack1,
                ante_i32,
            );

        let pack2 =
            next_pack(&mut pack_inst, ante_i32);
        let pack2_contents =
            open_pack_detailed(
                &mut pack_inst,
                pack2,
                ante_i32,
            );
        let pack3 =
    next_pack(&mut pack_inst, ante_i32);

let pack3_contents =
    open_pack_detailed(
        &mut pack_inst,
        pack3,
        ante_i32,
    );

let pack4 =
    next_pack(&mut pack_inst, ante_i32);

let pack4_contents =
    open_pack_detailed(
        &mut pack_inst,
        pack4,
        ante_i32,
    );

        out.push('{');

        out.push_str("\"ante\":");
        out.push_str(&ante.to_string());

        out.push_str(",\"boss\":\"");
        out.push_str(&json_escape(boss));
        out.push('"');

        out.push_str(",\"voucher\":\"");
        out.push_str(&json_escape(voucher));
        out.push('"');

        out.push_str(",\"small_tag\":\"");
        out.push_str(&json_escape(small_tag));
        out.push('"');

        out.push_str(",\"big_tag\":\"");
        out.push_str(&json_escape(big_tag));
        out.push('"');

        // Shop
        out.push_str(",\"shop\":[");

write_shop_slot(&mut out, &shop1);
out.push(',');
write_shop_slot(&mut out, &shop2);
out.push(',');
write_shop_slot(&mut out, &shop3);
out.push(',');
write_shop_slot(&mut out, &shop4);
out.push(',');
write_shop_slot(&mut out, &shop5);
out.push(',');
write_shop_slot(&mut out, &shop6);

out.push(']');

        // Packs
   out.push_str(",\"packs\":[");

write_pack(&mut out, pack1, &pack1_contents);
out.push(',');

write_pack(&mut out, pack2, &pack2_contents);
out.push(',');

write_pack(&mut out, pack3, &pack3_contents);
out.push(',');

write_pack(&mut out, pack4, &pack4_contents);

out.push(']');

        out.push('}');
    }

    out.push_str("]}");

    out
}

fn shop_kind_name(
    kind: ShopItemType,
) -> &'static str {
    match kind {
        ShopItemType::Joker => "Joker",
        ShopItemType::Tarot => "Tarot",
        ShopItemType::Planet => "Planet",
        ShopItemType::PlayingCard => "Playing Card",
        ShopItemType::Spectral => "Spectral",
    }
}

fn rarity_name(
    rarity: Rarity,
) -> &'static str {
    match rarity {
        Rarity::Common => "Common",
        Rarity::Uncommon => "Uncommon",
        Rarity::Rare => "Rare",
        Rarity::Legendary => "Legendary",
    }
}

fn write_shop_slot(
    out: &mut String,
    slot: &crate::derive::ShopSlot,
) {
    out.push_str("{\"type\":\"");
    out.push_str(shop_kind_name(slot.kind));

    out.push_str("\",\"item\":\"");
    out.push_str(&json_escape(slot.item));

    out.push_str("\",\"edition\":\"");
    out.push_str(edition_name(slot.edition));

    out.push_str("\",\"rarity\":\"");

    if let Some(rarity) = slot.rarity {
        out.push_str(rarity_name(rarity));
    }

    out.push_str("\",\"eternal\":");
    out.push_str(
        if slot.stickers.eternal {
            "true"
        } else {
            "false"
        }
    );

    out.push_str(",\"perishable\":");
    out.push_str(
        if slot.stickers.perishable {
            "true"
        } else {
            "false"
        }
    );

    out.push_str(",\"rental\":");
    out.push_str(
        if slot.stickers.rental {
            "true"
        } else {
            "false"
        }
    );

    out.push('}');
}

fn write_pack(
    out: &mut String,
    pack_name: &str,
    contents: &PackContents,
) {
    out.push_str("{\"name\":\"");
    out.push_str(&json_escape(pack_name));
    out.push_str("\",\"contents\":[");

    match contents {
        PackContents::Tarots(items)
        | PackContents::Planets(items)
        | PackContents::Spectrals(items)
        | PackContents::Jokers(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }

                out.push('"');
                out.push_str(&json_escape(item));
                out.push('"');
            }
        }

        PackContents::StandardCards(cards) => {
            for (i, card) in cards.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }

                out.push('"');
                out.push_str(
                    &json_escape(card.base)
                );
                out.push('"');
            }
        }

        PackContents::Standard
        | PackContents::Unknown => {}
    }

    out.push_str("]}");
}

fn edition_name(e: Edition) -> &'static str {
    match e {
        Edition::None => "",
        Edition::Foil => "Foil",
        Edition::Holographic => "Holographic",
        Edition::Polychrome => "Polychrome",
        Edition::Negative => "Negative",
    }
}

fn fresh_instance(seed: &str, deck_idx: u8, stake_idx: u8) -> Instance {
    let mut inst = Instance::new(seed);
    inst.deck = deck_from_idx(deck_idx);
    inst.stake = stake_from_idx(stake_idx);
    inst
}

fn inspect_clause(c: &Clause, seed: &str, deck_idx: u8, stake_idx: u8) -> (bool, String) {
    match c {
        Clause::AnteShopHasJoker { ante, slot, joker, edition, sticker } => {
            let scan_end = if *slot == 255 { 16 } else { (*slot as usize) + 1 };
            let mut inst = fresh_instance(seed, deck_idx, stake_idx);
            for s in 0..scan_end {
                let sd = next_shop_item(&mut inst, *ante as i32);
                if !matches!(sd.kind, ShopItemType::Joker) { continue; }
                if sd.item != joker.as_str() { continue; }
                if let Some(e) = edition.as_deref() {
                    if !e.eq_ignore_ascii_case(edition_name(sd.edition)) { continue; }
                }
                if let Some(st) = sticker.as_deref() {
                    let ok = match st.to_ascii_lowercase().as_str() {
                        "eternal" => sd.stickers.eternal,
                        "perishable" => sd.stickers.perishable,
                        "rental" => sd.stickers.rental,
                        _ => true,
                    };
                    if !ok { continue; }
                }
                let mut extras = String::new();
                if !matches!(sd.edition, Edition::None) {
                    extras.push_str(" [");
                    extras.push_str(edition_name(sd.edition));
                    extras.push(']');
                }
                if sd.stickers.eternal { extras.push_str(" [Eternal]"); }
                if sd.stickers.perishable { extras.push_str(" [Perishable]"); }
                if sd.stickers.rental { extras.push_str(" [Rental]"); }
                return (true, format!("ante {} · shop slot {}{}", ante, s, extras));
            }
            (false, format!("no match in ante {} shop slots 0..{}", ante, scan_end - 1))
        }
        Clause::AnteTagIs { ante, position, tag } => {
            let mut inst = fresh_instance(seed, deck_idx, stake_idx);
            let mut drawn: &'static str = "";
            let p = *position as usize;
            for _ in 0..=p {
                drawn = next_tag(&mut inst, *ante as i32);
            }
            let ok = drawn == tag.as_str();
            let pos_label = if *position == 0 { "small blind" } else { "big blind" };
            if ok {
                (true, format!("ante {} · {} tag = {}", ante, pos_label, drawn))
            } else {
                (false, format!("ante {} · {} tag = {} (wanted {})", ante, pos_label, drawn, tag))
            }
        }
        Clause::AnteBossIs { ante, boss } => {
            let mut inst = fresh_instance(seed, deck_idx, stake_idx);
            let drawn = next_boss(&mut inst, *ante as i32);
            let ok = drawn == boss.as_str();
            if ok {
                (true, format!("ante {} boss = {}", ante, drawn))
            } else {
                (false, format!("ante {} boss = {} (wanted {})", ante, drawn, boss))
            }
        }
        Clause::VoucherIs { ante, voucher } => {
            let mut inst = fresh_instance(seed, deck_idx, stake_idx);
            let drawn = next_voucher(&mut inst, *ante as i32);
            let ok = drawn == voucher.as_str();
            if ok {
                (true, format!("ante {} voucher = {}", ante, drawn))
            } else {
                (false, format!("ante {} voucher = {} (wanted {})", ante, drawn, voucher))
            }
        }
        Clause::AntePackContains { ante, pack_index, card } => {
            let mut inst = fresh_instance(seed, deck_idx, stake_idx);
            let mut last_pack: &'static str = "";
            for _ in 0..=*pack_index {
                last_pack = next_pack(&mut inst, *ante as i32);
            }
            let contents = open_pack_detailed(&mut inst, last_pack, *ante as i32);
            let hit = pack_contents_contains(&contents, card);
            if hit {
                (true, format!("ante {} pack #{} ({}) contains {}", ante, pack_index, last_pack, card))
            } else {
                (false, format!("ante {} pack #{} ({}) does not contain {}", ante, pack_index, last_pack, card))
            }
        }
        Clause::AnteAnyPackContains { ante, max_packs, card } => {
            let mut inst = fresh_instance(seed, deck_idx, stake_idx);
            for i in 0..*max_packs {
                let pack = next_pack(&mut inst, *ante as i32);
                let contents = open_pack_detailed(&mut inst, pack, *ante as i32);
                if pack_contents_contains(&contents, card) {
                    return (true, format!("ante {} pack #{} ({}) contains {}", ante, i, pack, card));
                }
            }
            (false, format!("ante {} · no pack in first {} contained {}", ante, max_packs, card))
        }
        Clause::AnteSoulIs { ante, max_packs, joker } => {
            let mut inst = fresh_instance(seed, deck_idx, stake_idx);
            for i in 0..*max_packs {
                let pack = next_pack(&mut inst, *ante as i32);
                let contents = open_pack(&mut inst, pack, *ante as i32);
                if pack_has_item(&contents, "The Soul") {
                    let leg = resolve_soul_legendary(&mut inst, *ante as i32);
                    if leg == joker.as_str() {
                        return (true, format!("ante {} pack #{} ({}) · Soul → {}", ante, i, pack, leg));
                    } else {
                        return (false, format!("ante {} pack #{} ({}) · Soul → {} (wanted {})", ante, i, pack, leg, joker));
                    }
                }
            }
            (false, format!("ante {} · no Soul in first {} packs", ante, max_packs))
        }
        Clause::AnteWraithIs { ante, max_packs, joker } => {
            let mut inst = fresh_instance(seed, deck_idx, stake_idx);
            for i in 0..*max_packs {
                let pack = next_pack(&mut inst, *ante as i32);
                let contents = open_pack(&mut inst, pack, *ante as i32);
                if pack_has_item(&contents, "Wraith") {
                    let rare = resolve_wraith_rare(&mut inst, *ante as i32);
                    if rare == joker.as_str() {
                        return (true, format!("ante {} pack #{} ({}) · Wraith → {}", ante, i, pack, rare));
                    } else {
                        return (false, format!("ante {} pack #{} ({}) · Wraith → {} (wanted {})", ante, i, pack, rare, joker));
                    }
                }
            }
            (false, format!("ante {} · no Wraith in first {} packs", ante, max_packs))
        }
        Clause::AnteStandardCardIs { ante, max_packs, base, enhancement, edition, seal } => {
            let mut inst = fresh_instance(seed, deck_idx, stake_idx);
            for i in 0..*max_packs {
                let pack = next_pack(&mut inst, *ante as i32);
                let detailed = open_pack_detailed(&mut inst, pack, *ante as i32);
                if let PackContents::StandardCards(cards) = &detailed {
                    for (ci, card) in cards.iter().enumerate() {
                        if !base.is_empty() && card.base != base.as_str() { continue; }
                        if let Some(enh) = enhancement.as_deref() {
                            match card.enhancement {
                                Some(e) if e.eq_ignore_ascii_case(enh) => {}
                                _ => continue,
                            }
                        }
                        if let Some(ed) = edition.as_deref() {
                            if !ed.eq_ignore_ascii_case(edition_name(card.edition)) { continue; }
                        }
                        if let Some(s) = seal.as_deref() {
                            let card_seal = match card.seal {
                                crate::derive::Seal::None => "",
                                crate::derive::Seal::Red => "red",
                                crate::derive::Seal::Blue => "blue",
                                crate::derive::Seal::Gold => "gold",
                                crate::derive::Seal::Purple => "purple",
                            };
                            let want = s.to_ascii_lowercase();
                            let want_short = want.trim_end_matches(" seal");
                            if card_seal != want_short { continue; }
                        }
                        return (true, format!("ante {} pack #{} ({}) · card {} = {}", ante, i, pack, ci, card.base));
                    }
                }
            }
            (false, format!("ante {} · no matching standard card in first {} packs", ante, max_packs))
        }
        Clause::AnyOf { clauses } => {
            for (i, sub) in clauses.iter().enumerate() {
                let (ok, detail) = inspect_clause(sub, seed, deck_idx, stake_idx);
                if ok { return (true, format!("any_of[{}]: {}", i, detail)); }
            }
            (false, "no sub-clause matched".to_string())
        }
    }
}

fn pack_contents_contains(c: &PackContents, want: &str) -> bool {
    match c {
        PackContents::Tarots(v) | PackContents::Planets(v) |
        PackContents::Spectrals(v) | PackContents::Jokers(v) => v.iter().any(|x| *x == want),
        PackContents::StandardCards(cards) => cards.iter().any(|c| c.base == want),
        _ => false,
    }
}
fn pack_has_item(c: &PackContents, want: &str) -> bool {
    match c {
        PackContents::Tarots(v) | PackContents::Planets(v) |
        PackContents::Spectrals(v) | PackContents::Jokers(v) => v.iter().any(|x| *x == want),
        _ => false,
    }
}

fn deck_from_idx(i: u8) -> Deck {
    match i {
        0 => Deck::Red, 1 => Deck::Blue, 2 => Deck::Yellow, 3 => Deck::Green,
        4 => Deck::Black, 5 => Deck::Magic, 6 => Deck::Nebula, 7 => Deck::Ghost,
        8 => Deck::Abandoned, 9 => Deck::Checkered, 10 => Deck::Zodiac,
        11 => Deck::Painted, 12 => Deck::Anaglyph, 13 => Deck::Plasma,
        _ => Deck::Erratic,
    }
}
fn stake_from_idx(i: u8) -> Stake {
    match i {
        0 => Stake::White, 1 => Stake::Red, 2 => Stake::Green, 3 => Stake::Black,
        4 => Stake::Blue, 5 => Stake::Purple, 6 => Stake::Orange, _ => Stake::Gold,
    }
}

// ---------------------------------------------------------------------------
// V3 diagnostic surface — pairs with `engine/shaders/diagnostic.wgsl`.
// The browser worker runs both the WGSL compute shader and `v3_diagnostic_cpu`
// over the same parameters; if outputs match, the WebGPU stack is verified.
// ---------------------------------------------------------------------------

#[wasm_bindgen]
pub fn v3_diagnostic_cpu(seed_base: u32, iter_count: u32, seed_count: u32) -> Vec<u32> {
    crate::v3::diagnostic::run_cpu(seed_base, iter_count, seed_count)
}

/// Returns the WGSL shader source as a string so the JS side doesn't have to
/// fetch a separate file. Keeps engine and shader versioned together.
#[wasm_bindgen]
pub fn v3_diagnostic_shader_source() -> String {
    include_str!("../shaders/diagnostic.wgsl").to_string()
}
