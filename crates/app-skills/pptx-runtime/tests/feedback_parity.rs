use octos_pptx::Context;
use serde_json::json;

fn fixture(root: &std::path::Path) -> Context {
    let ctx = Context::new(root).unwrap();
    image::RgbImage::from_pixel(800, 450, image::Rgb([245, 240, 230]))
        .save(root.join("art.png"))
        .unwrap();
    ctx.write_json("source.json", &json!({"schema_version":1,
        "canvas":{"width_px":800,"height_px":450,"width_in":8,"height_in":4.5},
        "styles":{"body":{"font":"Inter","size_pt":20,"align":"center","anchor":"middle"}},
        "pages":[{"id":"scene","background":"art.png","copy":{"caption":"Visible ink"},
            "texts":[{"id":"caption","text":"Visible ink","style":"body","box":[100,120,300,100]}]}]})).unwrap();
    ctx
}

#[test]
fn operations_returns_all_results_pixels_and_stops_before_later_writes() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = fixture(dir.path());
    let result = ctx
        .execute(json!({"action":"operations","steps":[
        {"action":"crop","input":"art.png","box":[0,0,100,100],"output":"crop.png"},
        {"action":"image_info","input":"crop.png"}],"images":["crop.png"]}))
        .unwrap();
    assert_eq!(result["completed_steps"], 2);
    assert_eq!(result["results"][1]["result"]["width"], 100);
    assert_eq!(result["model_media"], json!(["crop.png"]));
    assert_eq!(result["quality_approved"], false);
    let failure = ctx
        .execute(json!({"action":"operations","steps":[
        {"action":"image_info","input":"crop.png"},
        {"action":"crop","input":"art.png","box":[799,449,10,10],"output":"bad.png"},
        {"action":"crop","input":"art.png","box":[0,0,10,10],"output":"must-not-exist.png"}]}))
        .unwrap();
    assert_eq!(failure["tool_success"], false);
    assert_eq!(failure["completed_steps"], 1);
    assert!(!dir.path().join("must-not-exist.png").exists());
    assert!(
        ctx.execute(json!({"action":"operations","steps":[{"action":"operations","steps":[]}]}))
            .is_err()
    );
}
