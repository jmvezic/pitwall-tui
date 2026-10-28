//! Deep merge of feed deltas into each topic's raw JSON value.
//!
//! Rules:
//! - object into object: merge key by key, recursively;
//! - scalar or array into anything: replace;
//! - object into array: keys are indices as strings; merge into that
//!   element, growing the array when the index is at (or past) its end;
//! - keys starting with `_` (such as `_kf`) are metadata and dropped.

use serde_json::Value;

pub fn merge(target: &mut Value, delta: Value) {
    match (target, delta) {
        (Value::Object(t), Value::Object(d)) => {
            for (k, v) in d {
                if is_meta(&k) {
                    continue;
                }
                match t.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        t.insert(k, strip_meta(v));
                    }
                }
            }
        }
        (Value::Array(t), Value::Object(d)) => {
            for (k, v) in d {
                if is_meta(&k) {
                    continue;
                }
                let Ok(i) = k.parse::<usize>() else {
                    tracing::debug!("non-index key {k:?} merged into array");
                    continue;
                };
                if i < t.len() {
                    merge(&mut t[i], v);
                } else {
                    t.resize(i, Value::Null);
                    t.push(strip_meta(v));
                }
            }
        }
        (target, delta) => *target = strip_meta(delta),
    }
}

fn is_meta(k: &str) -> bool {
    k.starts_with('_')
}

fn strip_meta(mut v: Value) -> Value {
    match &mut v {
        Value::Object(m) => {
            m.retain(|k, _| !is_meta(k));
            for x in m.values_mut() {
                *x = strip_meta(x.take());
            }
        }
        Value::Array(a) => {
            for x in a.iter_mut() {
                *x = strip_meta(x.take());
            }
        }
        _ => {}
    }
    v
}

/// Iterates the elements of something that may be an array or an object
/// keyed by index (a delta that arrived before any array existed), in
/// index order.
pub fn indexed(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(m) => {
            let mut items: Vec<(usize, &Value)> = m
                .iter()
                .filter_map(|(k, v)| Some((k.parse().ok()?, v)))
                .collect();
            items.sort_by_key(|(i, _)| *i);
            items.into_iter().map(|(_, v)| v).collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn merged(mut base: Value, deltas: &[Value]) -> Value {
        for d in deltas {
            merge(&mut base, d.clone());
        }
        base
    }

    #[test]
    fn object_merge_is_recursive() {
        // Real TimingData deltas from the 2025 Australian GP.
        let base = json!({"Lines": {"4": {"GapToLeader": "", "IntervalToPositionAhead": {"Value": "", "Catching": false}, "NumberOfLaps": 0}}});
        let v = merged(
            base,
            &[
                json!({"Lines": {"4": {"GapToLeader": "LAP 1", "IntervalToPositionAhead": {"Value": "LAP 1"}}}}),
            ],
        );
        assert_eq!(v["Lines"]["4"]["GapToLeader"], "LAP 1");
        assert_eq!(v["Lines"]["4"]["IntervalToPositionAhead"]["Value"], "LAP 1");
        assert_eq!(
            v["Lines"]["4"]["IntervalToPositionAhead"]["Catching"],
            false
        );
        assert_eq!(v["Lines"]["4"]["NumberOfLaps"], 0);
    }

    #[test]
    fn index_keys_merge_into_arrays() {
        // TimingAppData: snapshot has an empty Stints array, deltas index into it.
        let v = merged(
            json!({"Lines": {"4": {"Stints": []}}}),
            &[
                json!({"Lines": {"4": {"Stints": {"0": {"Compound": "INTERMEDIATE", "New": "true", "TotalLaps": 0}}}}}),
                json!({"Lines": {"4": {"Stints": {"0": {"TotalLaps": 7}}}}}),
                json!({"Lines": {"4": {"Stints": {"1": {"Compound": "HARD", "New": "true", "TotalLaps": 0}}}}}),
            ],
        );
        let stints = v["Lines"]["4"]["Stints"].as_array().unwrap();
        assert_eq!(stints.len(), 2);
        assert_eq!(stints[0]["TotalLaps"], 7);
        assert_eq!(stints[0]["Compound"], "INTERMEDIATE");
        assert_eq!(stints[1]["Compound"], "HARD");
    }

    #[test]
    fn race_control_messages_append() {
        let v = merged(
            json!({"Messages": [{"Message": "LOW GRIP CONDITIONS"}]}),
            &[json!({"Messages": {"1": {"Category": "Drs", "Message": "DRS DISABLED"}}})],
        );
        assert_eq!(v["Messages"][1]["Message"], "DRS DISABLED");
    }

    #[test]
    fn gaps_are_padded_and_scalars_replace() {
        let v = merged(
            json!({"a": [1], "b": {"x": 1}}),
            &[json!({"a": {"3": 4}, "b": 5})],
        );
        assert_eq!(v["a"], json!([1, null, null, 4]));
        assert_eq!(v["b"], 5);
        let v = merged(json!({"a": [1, 2, 3]}), &[json!({"a": [9]})]);
        assert_eq!(v["a"], json!([9]));
    }

    #[test]
    fn metadata_keys_are_dropped() {
        let v = merged(
            json!({}),
            &[json!({"_kf": true, "Lines": {"1": {"_deleted": ["x"], "Position": "2"}}})],
        );
        assert_eq!(v, json!({"Lines": {"1": {"Position": "2"}}}));
    }

    #[test]
    fn indexed_handles_both_shapes() {
        let a = json!([1, 2]);
        let o = json!({"1": "b", "0": "a", "10": "c"});
        assert_eq!(indexed(&a).len(), 2);
        assert_eq!(indexed(&o), vec![&json!("a"), &json!("b"), &json!("c")]);
    }
}
