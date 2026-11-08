---
name: write-in-turkish
description: Write, translate or rewrite anything in Turkish (Türkçe): messages, announcements, emails, docs, UI strings, article passages, replies to Turkish speakers. Use this whenever the result must be in Turkish or the user asks for a translation to or from Turkish, even when they do not say "translate", and even for a few sentences. Claude drafts in English and the codex CLI (an OpenAI model, which writes far more natural Turkish) produces the Turkish text.
---

# Write in Turkish

Claude's own Turkish reads translated: stiff word order, off idioms, formal when it should not be.
OpenAI's models write Turkish like a native, and this box has the `codex` CLI logged in. So do
what each is good at: you decide what to say, codex says it in Turkish.

## Workflow

1. **Write the final text in English first**, complete and exactly as it should be published:
   structure, headings, code, links, placeholders. Translation is the last step, not a draft stage.
   Decide the register while writing: who reads it, and whether they are addressed as *sen*
   (informal) or *siz* (formal or plural). If it is unclear and matters, ask the user once.

2. **Translate with the bundled script**, the English on stdin, your notes on register, audience
   and terms to keep untranslated in `--note`:

   ```bash
   scripts/translate.sh --note "siz, product users, keep 'drop box' and 'agentbox' untranslated" < draft.md > tr.md
   ```

   The script path is relative to this skill's folder. `--to en` translates Turkish into English
   the same way (a Turkish email, article or bug report the user wants read or answered).

3. **Check the Turkish against your English** before delivering: every paragraph present, nothing
   added, code and placeholders byte-for-byte intact, names and numbers unchanged, the register
   you asked for. Fix a small slip yourself (a dropped link, a placeholder changed); for a wrong
   tone or a mistranslated term rerun the script with a sharper `--note`. You can read Turkish
   well; the gap is only in writing it, so use that for the review.

4. **Deliver the Turkish only**, unless the user asked to see both. Say in one line that the
   Turkish was produced through codex, so the user knows what to review if something reads off.

## Long texts

An article longer than a few thousand words goes through in pieces: split at headings, translate
each piece with the same `--note` (add "part N of M, same terminology as before"), then join.
Keep the pieces whole sections, so codex sees the context of what it translates.

## When codex fails

A non-zero exit means no translation was produced: not logged in, no network, or the model
refused. Report the error line. Then write the Turkish yourself and mark it as unreviewed by
codex, so the user knows to read it more carefully. Do not silently deliver your own Turkish.
