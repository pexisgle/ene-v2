# 心・自動記憶・権限の調査資料

調査日：2026-11-03。先行調査の67件の識別子・原典・確認範囲を採録する。67製品の実機検証や67論文の全文精読を意味しない。今回の文書化で全件を再取得したという主張でもない。ソースの版が指定されていないURLは変化し得る。

実装・公式説明・研究・開発実験を区別する。要旨や検索掲載本文のみの資料は評価観点・追加調査の入口であり、単独で実装の正しさや性能を保証する根拠にはしない。取得に失敗した本文を確認済みにしない。原文・コード・データ・台詞・資産は転載せず、具体的な移植時にライセンスを確認する。

採用・非採用の判断は[参考元のAlpha節](../references.md#alpha-design-sources)、規範は[アーキテクチャ](../architecture.md)と[心の設計](../design/mind.md)に集約する。この一覧自体を製品要件にしない。

## キャラクター・AITuber・ロールプレイ製品（20件）

<a id="p01"></a>
### P01 — AIRI — Overview

[原典](https://airi.moeru.ai/docs/en/docs/overview/)。確認範囲：公式概要の検索掲載本文。

<a id="p02"></a>
### P02 — AIRI — Character card template

[原典](https://airi.moeru.ai/docs/en/docs/manual/tamagotchi/character-card-template)。確認範囲：公式文書の関連本文。

<a id="p03"></a>
### P03 — AIRI — Skills, automations, and long-term memory ADR

[原典](https://github.com/moeru-ai/airi/blob/b2f3e7d327b96b3c012f39f4eb5543c06710b48b/docs/ai/adr/2026-10-10-skills-automations-and-memory.md)。確認範囲：一次設計文書の本文。

<a id="p04"></a>
### P04 — AIRI — memory tools implementation

[原典](https://github.com/moeru-ai/airi/blob/b2f3e7d327b96b3c012f39f4eb5543c06710b48b/packages/stage-ui/src/tools/memory.ts)。確認範囲：関連ソース全文。

<a id="p05"></a>
### P05 — AIRI — FlowChat memory experiment

[原典](https://airi.moeru.ai/docs/en/blog/DevLog-2026.01.01/)。確認範囲：開発ブログの関連本文。

<a id="p06"></a>
### P06 — AIRI — Layered memory development notes

[原典](https://airi.moeru.ai/docs/en/blog/DevLog-2025.04.14/)。確認範囲：開発ブログの関連本文。

<a id="p07"></a>
### P07 — AITuberKit — Character settings

[原典](https://docs.aituberkit.com/en/guide/character/common)。確認範囲：公式文書の関連本文。

<a id="p08"></a>
### P08 — AITuberKit — Memory settings

[原典](https://docs.aituberkit.com/guide/memory-settings)。確認範囲：公式文書の関連本文。

<a id="p09"></a>
### P09 — AITuber OnAir Core — Memory, expression, events

[原典](https://github.com/shinshin86/aituber-onair/blob/main/packages/core/README.md)。確認範囲：公式READMEの関連部分。

<a id="p10"></a>
### P10 — Open-LLM-VTuber — README

[原典](https://github.com/Open-LLM-VTuber/Open-LLM-VTuber/blob/main/README.md)。確認範囲：公式README本文。

<a id="p11"></a>
### P11 — SillyTavern — Character Design

[原典](https://docs.sillytavern.app/usage/core-concepts/characterdesign/)。確認範囲：公式文書本文。

<a id="p12"></a>
### P12 — SillyTavern — World Info

[原典](https://docs.sillytavern.app/usage/core-concepts/worldinfo/)。確認範囲：公式文書の関連本文。

<a id="p13"></a>
### P13 — SillyTavern — Prompt Manager

[原典](https://docs.sillytavern.app/usage/prompts/prompt-manager/)。確認範囲：公式ページの検索掲載本文。

<a id="p14"></a>
### P14 — SillyTavern — Character Expressions

[原典](https://docs.sillytavern.app/extensions/expression-images/)。確認範囲：公式文書本文。

<a id="p15"></a>
### P15 — SillyTavern — Summarize

[原典](https://docs.sillytavern.app/extensions/summarize/)。確認範囲：公式文書の関連本文。

<a id="p16"></a>
### P16 — SillyTavern — Chat Vectorization

[原典](https://docs.sillytavern.app/extensions/chat-vectorization/)。確認範囲：公式ページの検索掲載本文。

<a id="p17"></a>
### P17 — RisuAI — HypaMemory V3

[原典](https://github.com/kwaroran/Risuai/blob/main/src/ts/process/memory/hypav3.ts)。確認範囲：関連ソース先頭230行。

<a id="p18"></a>
### P18 — Character Card V3 specification

[原典](https://github.com/kwaroran/character-card-spec-v3/blob/main/SPEC_V3.md)。確認範囲：公式仕様の検索掲載本文。

<a id="p19"></a>
### P19 — NovelAI — Lorebook

[原典](https://docs.novelai.net/en/text/lorebook/)。確認範囲：公式文書の関連本文。

<a id="p20"></a>
### P20 — Kindroid — Memory documentation

[原典](https://kindroid.ai/v2/settings/?article=memory&section=help)。確認範囲：公式ページの検索掲載本文（直接再取得失敗）。

## 人格・感情・長期ロールプレイ研究（15件）

<a id="r01"></a>
### R01 — RoleLLM

[原典](https://aclanthology.org/2024.findings-acl.878/)。確認範囲：出版社ページの要旨。

<a id="r02"></a>
### R02 — ChatHaruhi

[原典](https://arxiv.org/abs/2308.09597)。確認範囲：著者公開要旨。

<a id="r03"></a>
### R03 — MRPrompt / MRBench

[原典](https://aclanthology.org/2026.findings-acl.1175/)。確認範囲：HTML論文の手法・関連節。

<a id="r04"></a>
### R04 — Digital Life Project / SocioMind

[原典](https://arxiv.org/html/2312.04547)。確認範囲：HTML論文の設計・実装説明・関連節。

<a id="r05"></a>
### R05 — DLP-SocioMind — official implementation

[原典](https://github.com/AlanJiang98/DLP-SocioMind/blob/main/README.md)。確認範囲：公式実装READMEの関連本文。

<a id="r06"></a>
### R06 — DualMem / RoleMemo — From Facts to Insights

[原典](https://arxiv.org/html/2605.25693)。確認範囲：HTML論文の方法・限界・関連節。

<a id="r07"></a>
### R07 — PersonaTree

[原典](https://arxiv.org/html/2606.04780)。確認範囲：HTML論文の方法・関連節。

<a id="r08"></a>
### R08 — Feeling First, Speaking Second

[原典](https://aclanthology.org/volumes/2026.cas-1/)。確認範囲：出版社の論文集ページに掲載された要旨。

<a id="r09"></a>
### R09 — Generative Agents

[原典](https://arxiv.org/abs/2304.03442)。確認範囲：著者公開要旨。

<a id="r10"></a>
### R10 — InCharacter

[原典](https://arxiv.org/abs/2310.17976)。確認範囲：著者公開要旨。

<a id="r11"></a>
### R11 — CharacterEval

[原典](https://aclanthology.org/2024.acl-long.638/)。確認範囲：出版社ページの要旨。

<a id="r12"></a>
### R12 — REMP: Relationship–Emotion–Memory–Persona

[原典](https://aclanthology.org/2026.findings-emnlp.602/)。確認範囲：出版社ページの検索掲載要旨（本文再取得失敗）。

<a id="r13"></a>
### R13 — Persistent Personas?

[原典](https://aclanthology.org/2026.eacl-long.246/)。確認範囲：出版社ページの要旨。

<a id="r14"></a>
### R14 — Rethinking Role-Playing Evaluation: Anonymous Benchmarking

[原典](https://aclanthology.org/2026.sigdial-1.15/)。確認範囲：出版社ページの要旨。

<a id="r15"></a>
### R15 — Situation–Internal State–Behaviorによる人格評価

[原典](https://aclanthology.org/2026.findings-emnlp.745/)。確認範囲：出版社ページの検索掲載要旨。

## 自動記憶・想起・記憶評価（17件）

<a id="m01"></a>
### M01 — Letta — Memory & dreaming

[原典](https://docs.letta.com/configuration/memory)。確認範囲：公式文書本文。

<a id="m02"></a>
### M02 — Letta — MemFS

[原典](https://docs.letta.com/concepts/memfs)。確認範囲：公式文書の関連本文。

<a id="m03"></a>
### M03 — LangMem — Core concepts

[原典](https://langchain-ai.github.io/langmem/concepts/conceptual_guide/)。確認範囲：公式文書の関連本文。

<a id="m04"></a>
### M04 — LangMem — Delayed background memory processing

[原典](https://langchain-ai.github.io/langmem/guides/delayed_processing/)。確認範囲：公式文書本文。

<a id="m05"></a>
### M05 — LangMem — Hot path memory tools

[原典](https://langchain-ai.github.io/langmem/hot_path_quickstart/)。確認範囲：公式ページの検索掲載本文。

<a id="m06"></a>
### M06 — Mem0: Building Production-Ready AI Agents with Scalable Long-Term Memory

[原典](https://arxiv.org/html/2504.19413v1)。確認範囲：HTML論文の方法・関連節。

<a id="m07"></a>
### M07 — Mem0 — Open source overview

[原典](https://docs.mem0.ai/open-source/overview)。確認範囲：公式文書の関連本文。

<a id="m08"></a>
### M08 — Graphiti — Overview

[原典](https://help.getzep.com/v2/graphiti/getting-started/overview)。確認範囲：公式バージョン付きページの検索掲載本文。

<a id="m09"></a>
### M09 — Zep — How graph creation works

[原典](https://help.getzep.com/how-graph-creation-works)。確認範囲：公式文書の関連本文。

<a id="m10"></a>
### M10 — A-Mem

[原典](https://arxiv.org/html/2502.12110)。確認範囲：HTML論文の方法・関連節。

<a id="m11"></a>
### M11 — Nemori / What Deserves Memory

[原典](https://arxiv.org/html/2508.03341)。確認範囲：HTML論文の方法・関連節。

<a id="m12"></a>
### M12 — MemoryBank

[原典](https://arxiv.org/abs/2305.10250)。確認範囲：著者公開要旨。

<a id="m13"></a>
### M13 — FlexA-Mem

[原典](https://arxiv.org/abs/2610.33274)。確認範囲：著者公開要旨。

<a id="m14"></a>
### M14 — APEX-MEM

[原典](https://arxiv.org/abs/2604.14362)。確認範囲：著者公開要旨。

<a id="m15"></a>
### M15 — LongMemEval

[原典](https://arxiv.org/abs/2410.10813)。確認範囲：著者公開要旨。

<a id="m16"></a>
### M16 — LongMemEval-V2

[原典](https://arxiv.org/abs/2605.12493)。確認範囲：著者公開要旨。

<a id="m17"></a>
### M17 — LoCoMo — Evaluating very long-term conversational memory

[原典](https://aclanthology.org/2024.acl-long.747/)。確認範囲：出版社要旨と著者プロジェクト説明。

## Rig・権限・サンドボックス（15件）

<a id="s01"></a>
### S01 — Rig v0.46.0 release

[原典](https://github.com/0xPlaygrounds/rig/releases/tag/v0.46.0)。確認範囲：公式リリース本文。

<a id="s02"></a>
### S02 — Rig — rig-agent README

[原典](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-agent/README.md)。確認範囲：公式READMEの関連本文。

<a id="s03"></a>
### S03 — Rig — rig-ecs README

[原典](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-ecs/README.md)。確認範囲：公式README本文。

<a id="s04"></a>
### S04 — Rig — rig-memory README

[原典](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-memory/README.md)。確認範囲：公式README本文。

<a id="s05"></a>
### S05 — Rig — Plugin guide

[原典](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-harness/PLUGINS.md)。確認範囲：公式設計ガイドの関連節。

<a id="s06"></a>
### S06 — Rig — ApprovalPlugin implementation

[原典](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/plugins/rig-basics/src/approval.rs)。確認範囲：関連ソース全文。

<a id="s07"></a>
### S07 — Rig — ShellSandbox corpus example

[原典](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/corpus/base/src/shell_sandbox.rs)。確認範囲：関連ソース先頭240行。

<a id="s08"></a>
### S08 — Anthropic Sandbox Runtime (srt)

[原典](https://github.com/anthropics/sandbox-runtime/blob/main/README.md)。確認範囲：公式README先頭250行。

<a id="s09"></a>
### S09 — Claude Code — Sandboxing

[原典](https://code.claude.com/docs/en/sandboxing)。確認範囲：公式文書の関連本文。

<a id="s10"></a>
### S10 — Claude Code — Permissions

[原典](https://code.claude.com/docs/en/permissions)。確認範囲：公式文書の関連本文。

<a id="s11"></a>
### S11 — Codex — Agent approvals and security

[原典](https://learn.chatgpt.com/docs/agent-approvals-security)。確認範囲：公式文書の関連本文。

<a id="s12"></a>
### S12 — Codex — Sandboxing

[原典](https://learn.chatgpt.com/docs/sandboxing)。確認範囲：公式文書の関連本文。

<a id="s13"></a>
### S13 — OpenCode — Permissions

[原典](https://opencode.ai/docs/permissions/)。確認範囲：公式文書の関連本文。

<a id="s14"></a>
### S14 — MCP — Tool annotations

[原典](https://blog.modelcontextprotocol.io/posts/2026-03-16-tool-annotations/)。確認範囲：公式記事の検索掲載本文。

<a id="s15"></a>
### S15 — Cedar policy language

[原典](https://docs.cedarpolicy.com/)。確認範囲：公式文書の概要・関連部分。
