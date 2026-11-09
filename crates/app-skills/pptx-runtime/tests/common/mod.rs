//! Explicit judgments for synthetic fixtures, never used by production tools.
use octos_pptx::{Context, object_review, review};
use serde_json::{Value, json};
pub fn object_judgments(ctx: &Context, name: &str) -> Value {
    let (cycle, spec) =
        review::bound_cycle(ctx, &ctx.path(format!("{name}/cycle.json")).unwrap()).unwrap();
    json!(object_review::inventory(&cycle,&spec).unwrap().iter().map(|v| {
        let checks:serde_json::Map<String,Value>=object_review::CHECKS.iter().map(|k|(k.to_string(),json!({"status":"pass","observation":"Synthetic fixture has an explicit expected arrangement; this is test data, not a production visual approval."}))).collect();
        let mut row=json!({"page":v["page"],"object_id":v["object_id"],"evidence_hash":v["evidence_hash"],"checks":checks});
        if v["design_available"]==true {
            row["fidelity"]=json!({"evidence":octos_pptx::fidelity::evidence(v),"comparison_reliable":true,
                "layout_relation":"preserved","reference_interference":"absent","current_interference":"absent",
                "interference_change":"equivalent","basis":"Synthetic fixture supplies the intended arrangement; isolated gate tests author these facts explicitly"});
        }
        row
    }).collect::<Vec<_>>())
}
