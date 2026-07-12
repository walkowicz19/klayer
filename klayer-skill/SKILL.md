---
name: klayer-skill
description: Automatically use klayer for grounding, codebase indexing, session memory, and episode logging during software development tasks unless the user explicitly opts out.
---

# Klayer Skill

This skill guides AI agents to automatically utilize `klayer` for grounding, code search, and session logging during all stages of the Software Development Life Cycle (SDLC).

## Core Principles

1. **Automatic Context Grounding**:
   - Unless the user explicitly requests not to use codebase or session memory during a session, always start by checking if a codebase is indexed using `list_repos`.
   - Run `recall_session(repo)` at the start of a task to recover context, decisions, and mistakes to avoid.
   - Run `search_code(query)` for relevant symbols, patterns, or files before modifying or writing code.
   - Run `recall(domain, query)` before answering questions that touch a registered domain.

2. **Continuous Session Logging (SDLC Log)**:
   - At the completion of any task, feature implementation, bug fix, or refactor, always call `log_work(repo, kind, title, body)` to record the work done.
   - Document key technical decisions, traps, and errors to prevent future models from repeating the same mistakes. Use the following categories:
     - `done`: Accomplishments and completed features.
     - `decision`: Structural or architectural choices and their rationale.
     - `avoid`: Mistakes, errors, or anti-patterns to avoid.
     - `failed`: Unsuccessful attempts and details of why they failed.
     - `note`: Useful context or configuration details.

3. **Mandatory Prompts for Indexing & Logs**:
   - Always ask/prompt the user to index the codebase (`index_codebase`) when files are modified, added, or deleted, ensuring the persistent code database is synced.
   - Ask the user to log/manage episode logs (`log_episode`) to keep track of execution traces during testing or execution.

4. **Governance & Enforced Domains**:
   - Before calling `execute_change` against a domain, call `recall(domain, query)` against that same domain first in the same run — if the domain is registered as `enforced`, `execute_change` refuses without a prior `recall` in that run unless `override: true` is explicitly passed, and every override is logged for later audit.
   - When registering or editing a domain with `register_domain`, set `enforced: true` for domains whose knowledge must not be silently ignored by an agent (compliance rules, security policy, architectural constraints).
   - If a `recall` response contains a `MANDATORY RULE` prefix, treat that item as binding guidance for the current task, not optional context.
   - If a knowledge conflict is flagged (`conflict_with_id`/`conflict_status` on a `recall`/`list_conflicts` result), resolve it via `resolve_conflict` or surface it to the user rather than silently trusting either version.

5. **Compliance & Audit Export**:
   - Use `export_explainability(format: "json" | "pdf")` to produce an audit trail of which knowledge grounded a set of episodes, including trust tier, source, and approval history. Default to `"json"` for programmatic use; use `"pdf"` when the user wants a document to hand off.
   - The report also surfaces compliance gaps (enforced-domain actions taken without a prior `recall`, or with `override: true`) — check these before telling a user a run was fully compliant.

6. **Media Evidence**:
   - Use `ingest_media` to attach a screenshot, mockup, or other image as evidence for a `knowledge_id` (inherits that item's trust tier) or as standalone media scoped to a `domain` (stays unpromoted until linked). Use `attach_media` to link previously-standalone media to a knowledge item later. Use `list_media` to check what's already attached before re-ingesting the same evidence. Images only for now — do not attempt to ingest video.

7. **Model Routing Configuration**:
   - Use `configure_model_registry` (`add_model` / `add_sub_agent` / `add_routing_rule` / `update` / `remove`) to manage the per-harness model/routing registry — every call previews the change first and requires a second call with `confirm: true` before it's persisted, since a wrong entry silently affects every future recommendation.
   - Use `estimate_task_complexity` with an explicit `repo` param when the recommendation should be scoped to one indexed repository rather than global codebase stats; `harness` is optional and falls back to the connecting client's own identity when omitted.

8. **Observability**:
   - When self-reporting is available, pass `model`, `tokens_used`, and `cost` on `recall`/`remember`/`ingest` calls — these are best-effort and optional (MCP has no standard field for this), but improve the accuracy of the Usage & Cost dashboard when supplied.

9. **PII Redaction**:
   - By default, every domain redacts pattern-matched PII (emails, phone numbers, card numbers, ID-shaped digit sequences) in `remember`/`propose`/`ingest`/`log_work` content before it's stored — a recalled item containing `[REDACTED:EMAIL]`, `[REDACTED:CARD]`, `[REDACTED:PHONE]`, or `[REDACTED:ID_NUMBER]` reflects the original having matched one of these patterns, not a bug or missing content. Don't try to "fix" or re-insert what was redacted.
   - A domain can only skip redaction if `register_domain` was called with `redact_enabled: false` — deliberately rare (structured PII under proper access control). Do not set this without an explicit user request.

10. **Retention (TTL)**:
    - `register_domain`'s `retention_days` sets how long that domain's knowledge lives before a background sweep purges it (`clear_retention: true` removes the limit again). `set_knowledge_retention(id, retention_days)` overrides the domain default for one item. Default is no expiration — don't set a retention window unless the user asks for one, since it's a real, logged deletion, not a soft flag.
    - Domains created from a Marketplace template are excluded from retention by default (shared reference content), even if the surrounding domain has a retention policy — only an explicit `retention_days` set directly on that template domain overrides this.
