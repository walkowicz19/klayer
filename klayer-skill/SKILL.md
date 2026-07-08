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
