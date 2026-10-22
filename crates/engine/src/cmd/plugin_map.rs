//! Replacing the AAX plug-ins a Pro Tools session used with VST3 or CLAP ones.
//!
//! The session report only names the AAX plug-ins on each track (no settings), so a replacement
//! is inserted at its defaults. `file.apply_plugin_map` takes the user's choices and inserts them
//! as one undoable step.

use super::*;
use crate::cmd;
use serde_json::json;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "file.apply_plugin_map",
        "Apply Plug-in Map",
        [],
        None,
        "{map: [{aax: name, plugin: id | null, tracks: [track name]}]} → {inserted: [{aax, track, slot, plugin}], failed: [{aax, track, error}], skipped: n} — inserts each chosen plug-in (`vst3:…`, `clap:…`) at its defaults in the first free slot of every listed track, in map order, as one undo step; null plug-ins are skipped; entries that cannot be applied are reported in `failed` and change nothing",
        always,
        apply
    )]
}

const MAX_MAP_ENTRIES: usize = 2_000;
const MAX_TRACKS_PER_ENTRY: usize = 256;

fn apply(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "file.apply_plugin_map";
    let map = p.get("map").and_then(Value::as_array).ok_or_else(|| bad(CMD, "`map` required"))?;
    if map.len() > MAX_MAP_ENTRIES {
        return Err(bad(CMD, format!("`map` has more than {MAX_MAP_ENTRIES} entries")));
    }
    let mut inserted = Vec::new();
    let mut failed = Vec::new();
    let mut skipped = 0usize;
    for entry in map {
        let aax = str_param(entry, "aax").unwrap_or_default();
        let plugin = match entry.get("plugin") {
            None | Some(Value::Null) => {
                skipped += 1;
                continue;
            }
            Some(Value::String(id)) => id.as_str(),
            Some(_) => return Err(bad(CMD, format!("`plugin` for `{aax}` must be an id string or null"))),
        };
        let names: Vec<&str> = entry
            .get("tracks")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).take(MAX_TRACKS_PER_ENTRY).collect())
            .unwrap_or_default();
        let Some(info) = super::clap::plugin_info(plugin) else {
            for name in names {
                failed.push(json!({"aax": aax, "track": name, "error": format!("unknown plugin `{plugin}`")}));
            }
            continue;
        };
        for name in names {
            let Some(id) = e.session().track_by_name(name).map(|t| t.id) else {
                failed.push(json!({"aax": aax, "track": name, "error": "no such track"}));
                continue;
            };
            let Some(track) = e.session_mut().track_mut(id) else { continue };
            let Some(slot) = track.mixer.inserts.iter().position(Option::is_none) else {
                failed.push(json!({"aax": aax, "track": name, "error": "all insert slots are in use"}));
                continue;
            };
            if let Some(cell) = track.mixer.inserts.get_mut(slot) {
                *cell = Some(super::mix::default_insert(info));
            }
            inserted.push(json!({"aax": aax, "track": name, "slot": slot, "plugin": info.id}));
        }
    }
    Ok(json!({"inserted": inserted, "failed": failed, "skipped": skipped}))
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    fn engine() -> Engine {
        let mut e = Engine::default();
        e.execute("track.new", &json!({"count": 2, "name": "Vox"})).unwrap();
        e
    }

    fn inserted(e: &Engine, track: usize) -> Vec<String> {
        e.session().tracks[track].mixer.inserts.iter().flatten().map(|i| i.plugin.clone()).collect()
    }

    #[test]
    fn inserts_on_matching_tracks_in_one_undo_step() {
        let mut e = engine();
        let names: Vec<String> = e.session().tracks.iter().map(|t| t.name.to_uppercase()).collect();
        let r = e
            .execute(
                "file.apply_plugin_map",
                &json!({"map": [
                    {"aax": "EQ3", "plugin": "gain", "tracks": [names[0], names[1]]},
                    {"aax": "Skipped", "plugin": null, "tracks": [names[0]]},
                    {"aax": "Comp", "plugin": "compressor", "tracks": [names[0]]},
                ]}),
            )
            .unwrap();
        assert_eq!(r["inserted"].as_array().map(Vec::len), Some(3), "{r}");
        assert_eq!(r["skipped"], 1);
        assert_eq!(inserted(&e, 0).len(), 2);
        assert_eq!(inserted(&e, 1).len(), 1);
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(inserted(&e, 0).is_empty() && inserted(&e, 1).is_empty());
    }

    #[test]
    fn failures_are_reported_and_the_rest_still_applies() {
        let mut e = engine();
        let first = e.session().tracks[0].name.clone();
        let r = e
            .execute(
                "file.apply_plugin_map",
                &json!({"map": [
                    {"aax": "A", "plugin": "vst3:does-not-exist", "tracks": [first]},
                    {"aax": "B", "plugin": "gain", "tracks": ["Nope", first]},
                ]}),
            )
            .unwrap();
        let failed = r["failed"].as_array().unwrap();
        assert_eq!(failed.len(), 2, "{r}");
        assert_eq!(r["inserted"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn a_full_track_is_reported_not_overwritten() {
        let mut e = engine();
        let first = e.session().tracks[0].name.clone();
        let tracks = [first.clone()];
        let map: Vec<_> = (0..11).map(|i| json!({"aax": format!("P{i}"), "plugin": "gain", "tracks": tracks})).collect();
        let r = e.execute("file.apply_plugin_map", &json!({"map": map})).unwrap();
        assert_eq!(r["inserted"].as_array().map(Vec::len), Some(10));
        assert_eq!(r["failed"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn bad_params_are_errors() {
        let mut e = engine();
        assert!(e.execute("file.apply_plugin_map", &json!({})).is_err());
        assert!(e.execute("file.apply_plugin_map", &json!({"map": [{"aax": "A", "plugin": 3, "tracks": []}]})).is_err());
    }
}
