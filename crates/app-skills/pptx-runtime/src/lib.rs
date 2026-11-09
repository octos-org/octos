//! A bounded native tool surface. Model judgment stays with the calling agent.
pub mod artwork_contact;
pub mod authoring;
pub mod check_feedback;
pub mod comparison_review;
pub mod composite_ink;
pub mod deck;
pub mod evidence;
pub mod feedback;
pub mod fidelity;
pub mod finding_guard;
pub mod fit;
pub mod fs;
pub mod geometry;
pub mod intent_evidence;
pub mod label_diff;
pub mod object_review;
pub mod ocr;
pub mod operations;
pub mod placement;
pub mod qa;
pub mod raster_overlap;
pub mod region_layout;
pub mod relations;
pub mod review;
pub mod review_reuse;
pub mod review_routes;
pub mod spec;
pub mod typography;
pub mod visual_diff;
pub mod visual_proof;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const ENGINE_REV: &str = "581ffb43217be6aa9850b23eea38473ded6043fd";
pub struct Context {
    pub root: PathBuf,
    pub continuation_owner: Option<String>,
}
impl Context {
    pub fn new(root: &Path) -> Result<Self> {
        let root = root.canonicalize()?;
        anyhow::ensure!(root.is_dir(), "workspace must be a directory");
        Ok(Self {
            root,
            continuation_owner: None,
        })
    }
    pub fn execute(&self, request: Value) -> Result<Value> {
        anyhow::ensure!(request.is_object(), "request must be a JSON object");
        match request
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("help")
        {
            "help" if request["topic"].as_str().is_some() => match request["topic"].as_str().unwrap() {
                "evidence_record" => Ok(json!({"action":"evidence_record","required":["cycle"],"optional":["review","comparisons","scope_change_reason"],
                    "example":{"action":"evidence_record","cycle":"attempt/cycle.json","review":"review/bound.json"},
                    "requirements":["Maker owns the frozen policy; delivery files must match the reviewed candidate.","Use independent review_submit output; omit review only to record genuinely incomplete machine-only evidence.","Rust increments a checked attempt; never invent or edit the counter.","Before the first completed check attempt=0 pending is expected, not a defect.","On the next tool turn, evidence_status confirms host consumption."],"quality_approved":false})),
                "review" | "finding_register" | "review_pack" | "review_submit" => Ok(json!({"topic":request["topic"],"documentation":include_str!("../references/review-lifecycle.md"),"quality_approved":false})),
                _=>bail!("Unknown help topic. Use evidence_record or review (finding_register/review_pack/review_submit). Omit topic for the full operation list."),
            },
            "help" => Ok(
                json!({"version":env!("CARGO_PKG_VERSION"),"engine_revision":ENGINE_REV,
                "actions":["operations","review_view","finding_register","doctor","build","cycle","render","inspect","review_pack","review_submit","qa_check","qa_help","fit","layout_search","layout_region","patch","split_text","represent_symbol","batch","commands","image_info","ocr","raster_overlap","compare","compare_cycle","crop","geometry","evidence_init","evidence_retarget","evidence_record","evidence_status"],
                "units":"spec geometry: canvas pixels; font: points; DeckCraft commands: points",
                "quality_approved":false,"fit":fit::usage(),"layout_search":placement::usage(),"layout_region":region_layout::usage(),"instructions":"Read the bundled pptx skill and rust-runtime reference for schemas."}),
            ),
            "doctor" => Ok(
                json!({"version":env!("CARGO_PKG_VERSION"),"engine_revision":ENGINE_REV,"runtime":"rust","ocr":ocr::capabilities(),"bundled_fonts":env!("OCTOS_PPTX_BUNDLED_FONTS")=="true","python_required":false,"libreoffice_required":false,"node_required":false,
                "continuation":{"host_identity_present":self.continuation_owner.is_some(),"host_identity":self.continuation_owner,"observer_consumption_verified":false,
                    "next":"Under Octos, require host_identity_present before evidence_init; omit owner_agent. After a checked cycle, require evidence_status.runtime_consumed_current_evidence on a subsequent tool turn. Identity injection alone does not verify the observer."},"quality_approved":false}),
            ),
            "build" | "cycle" => deck::build(self, &request),
            "render" => deck::render(self, &request),
            "inspect" => review::inspect(self, &request),
            "review_pack" => feedback::create(self, &request),
            "review_view" => operations::review_view(self, &request),
            "operations" => operations::run(self, &request),
            "review_submit" => object_review::submit(self, &request),
            "finding_register" => finding_guard::register(self, &request),
            "qa_check" => qa::check(self, &request),
            "qa_help" => Ok(json!({"algorithm":qa::ALGORITHM,
                "documentation":include_str!("../references/qa-gates.md"),"quality_approved":false})),
            "patch" => spec::patch(self, &request),
            "split_text" => authoring::split_text(self, &request),
            "represent_symbol" => authoring::represent_symbol(self, &request),
            "layout_region" => region_layout::propose(self, &request)
                .map_err(|e| anyhow::anyhow!("{e:#}\nUsage: {}", region_layout::usage())),
            "fit" => fit::propose(self, &request)
                .map_err(|e| anyhow::anyhow!("{e:#}\nUsage: {}", fit::usage())),
            "layout_search" => placement::search(self, &request)
                .map_err(|e| anyhow::anyhow!("{e:#}\nUsage: {}", placement::usage())),
            "batch" => deck::batch(self, &request),
            "commands" => deck::commands(&request),
            "compare" => review::compare(self, &request),
            "compare_cycle" => comparison_review::compare_cycle(self, &request),
            "crop" => review::crop(self, &request),
            "image_info" => review::image_info(self, &request),
            "ocr" => ocr::recognize(self, &request),
            "raster_overlap" => raster_overlap::run(self, &request),
            "geometry" => (|| {
                anyhow::ensure!(
                    request.get("spec").is_none() || request["spec"].is_string(),
                    "geometry spec must be the deck JSON file path, not a region object or array"
                );
                let mut result = geometry::proposal(&request)?;
                anyhow::ensure!(
                    request.get("spec").is_none() || request["image"].is_string(),
                    "geometry with spec requires the full-page image whose pixels were measured"
                );
                if let Some(path) = request["image"].as_str() {
                    let binding = self.binding(path)?;
                    let blank = review::load_image(self, &self.verify(&binding)?)?.to_rgb8();
                    let quad =
                        geometry::quad(request.get("region").or(request.get("quad")).unwrap())?;
                    if let Some(source) = request["spec"].as_str() {
                        let spec = spec::load(self, &self.path(source)?, None)?;
                        result = geometry::image_to_canvas_proposal(
                            &request,
                            [blank.width(), blank.height()],
                            [spec.canvas.width_px, spec.canvas.height_px],
                        )?;
                        result["spec"] = self.binding(self.path(source)?)?;
                        result["geometry_units"] = json!("canvas_pixels");
                    } else {
                        result["geometry_units"] = json!("source_image_pixels");
                        result["coordinate_warning"] = json!(
                            "These coordinates are image pixels, not necessarily spec canvas pixels. Supply spec with a full-page image for explicit conversion before using the proposal in page data."
                        );
                    }
                    result["owner_boundary"] = visual_proof::owner_boundary(&blank, quad);
                    result["owner_boundary_units"] = json!("source_image_pixels");
                    result["image"] = binding;
                }
                result["proposal_scope"] = json!("geometry of supplied coordinates; actual border and semantic ownership require verification");
                Ok(result)
            })().map_err(|e: anyhow::Error| anyhow::anyhow!("{e:#}\nUsage: {}", geometry::usage())),
            "evidence_init" => evidence::init(self, &request),
            "evidence_retarget" => evidence::retarget(self, &request),
            "evidence_record" => evidence::record(self, &request),
            "evidence_status" => evidence::status(self, &request),
            other => bail!("unknown action {other}"),
        }
    }
}

pub fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing or empty string {key}"))
}
