//! Media MCP tools: ingest_media, attach_media, list_media.

use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_router,
    ErrorData as McpError,
};

use crate::bootstrap::get_media_dir;
use super::{err, json_ok, text_ok, AttachMediaParams, IngestMediaParams, Klayer, ListMediaParams};

#[tool_router(router = media_tool_router, vis = "pub(crate)")]
impl Klayer {
    #[tool(
        description = "Ingest an image as a media attachment (Stage G: images only, video is out of scope). Accepts base64-encoded bytes + mime_type (image/png, image/jpeg, image/webp, image/gif). Pass knowledge_id to attach immediately (inherits that item's trust tier), or domain to store standalone (no trust tier until attach_media links it later)."
    )]
    fn ingest_media(
        &self,
        Parameters(p): Parameters<IngestMediaParams>,
    ) -> Result<CallToolResult, McpError> {
        if !kl_store::media::is_allowed_mime(&p.mime_type) {
            return Err(err(format!(
                "unsupported mime_type '{}': only image types are accepted in this stage ({})",
                p.mime_type,
                kl_store::media::ALLOWED_IMAGE_MIME_TYPES.join(", ")
            )));
        }
        if let Some(kid) = p.knowledge_id {
            if self.store.get_knowledge_by_id(kid).map_err(err)?.is_none() {
                return Err(err(format!("knowledge item #{kid} not found")));
            }
        }
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            p.data_base64.as_bytes(),
        )
        .map_err(|e| err(format!("invalid base64 data: {e}")))?;
        let path =
            kl_store::media::write_media(&get_media_dir(), &p.mime_type, &bytes).map_err(err)?;
        let storage_ref = path.to_string_lossy().to_string();
        let media_id = self
            .store
            .insert_media(
                &storage_ref,
                &p.mime_type,
                bytes.len() as i64,
                p.caption.as_deref(),
                p.knowledge_id,
                p.domain.as_deref(),
            )
            .map_err(err)?;
        let status = if p.knowledge_id.is_some() {
            "attached, trust inherited from knowledge item"
        } else {
            "standalone, unpromoted"
        };
        self.store
            .log_episode_auto(
                &self.session_run_id,
                Some("ingest_media"),
                Some(&format!(
                    "ingest_media mime_type={} bytes={}",
                    p.mime_type,
                    bytes.len()
                )),
                Some(&format!("stored media #{media_id} ({status})")),
                Some("success"),
                p.domain.as_deref(),
                None,
                None,
                None,
            )
            .ok();
        text_ok(format!(
            "Stored media #{media_id} ({} bytes, {}) at {storage_ref} — {status}.",
            bytes.len(),
            p.mime_type
        ))
    }

    #[tool(
        description = "Attach previously-standalone media to a knowledge item; the media's trust tier is updated to inherit that item's current tier."
    )]
    fn attach_media(
        &self,
        Parameters(p): Parameters<AttachMediaParams>,
    ) -> Result<CallToolResult, McpError> {
        if self
            .store
            .get_knowledge_by_id(p.knowledge_id)
            .map_err(err)?
            .is_none()
        {
            return Err(err(format!("knowledge item #{} not found", p.knowledge_id)));
        }
        let ok = self
            .store
            .attach_media(p.media_id, p.knowledge_id)
            .map_err(err)?;
        if !ok {
            return Err(err(format!("media #{} not found", p.media_id)));
        }
        text_ok(format!(
            "Attached media #{} to knowledge #{} (trust inherited).",
            p.media_id, p.knowledge_id
        ))
    }

    #[tool(description = "List media attachments, optionally filtered by domain or knowledge_id.")]
    fn list_media(
        &self,
        Parameters(p): Parameters<ListMediaParams>,
    ) -> Result<CallToolResult, McpError> {
        let rows = self
            .store
            .list_media(p.domain.as_deref(), p.knowledge_id)
            .map_err(err)?;
        json_ok(&rows)
    }
}
