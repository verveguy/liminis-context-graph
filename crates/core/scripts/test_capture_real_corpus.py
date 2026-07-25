#!/usr/bin/env python3
"""
test_capture_real_corpus.py — regression tests for the wikitext cleanup helpers in
capture_real_corpus.py. Run with: python3 crates/core/scripts/test_capture_real_corpus.py

Not part of the cargo test / CI gate (capture_real_corpus.py itself is an offline,
outside-CI tool — see its module docstring) — this is a standalone stdlib-only
`unittest` file for a maintainer to run locally when touching the cleanup regexes.
"""

import unittest

from capture_real_corpus import strip_templates, wikitext_to_prose

# Simple English Wikipedia, "2026 New Glenn rocket explosion" (rev 10877681), fetched
# verbatim. This revision's `<ref name=bbc />` self-closing tag, followed much later by a
# same-named `<ref name="bbc">{{Cite news|...}}</ref>` definition, previously triggered two
# compounding bugs: the ref regex's alternation order let the self-closing tag match the
# "has content" branch and greedily consume everything up to the next `</ref>` (including
# the Infobox's closing `}}`), and the resulting unbalanced `{{` then made the old
# depth-counting `strip_templates` drop the rest of the article. See #217.
NEW_GLENN_WIKITEXT = (
    "{{Use mdy dates}}\n{{Use American English|date=June 2026}}\n\n{{Infobox event\n"
    "| title = 2026 New Glenn rocket explosion\n"
    "| image = Administrator Isaacman Visits Blue Origin (NHQ20260529 admin 0004).jpg\n"
    "| caption = NASA Administrator [[Jared Isaacman]] observes the damage to Launch "
    "Complex 36 from a helicopter following the New Glenn explosion\n"
    "| date = {{start date|2026|05|28}}; {{days ago|2026|05|28}}\n"
    "| time = ~21:00<ref name=bbc />\n"
    "| timezone = [[UTC−04:00]]\n"
    "| Location = [[Cape Canaveral Space Force Station]], [[Florida]], [[United States]]\n"
    "| coordinates = {{coord|28.4718|N|80.5381|W|region:US-FL_type:event|"
    "display=title,inline}}\n"
    "| type = [[Explosion]]\n"
    "}}\n\n"
    "On May 28, 2026, a [[New Glenn]] rocket blew up during testing."
    "<ref>{{Cite web|date=2026-05-29|title=Moment Blue Origin rocket explodes during "
    "test in Florida|url=https://www.bbc.com/news/videos/cvgz0pdg32mo|"
    "access-date=2026-06-02|website=www.bbc.com|language=en-GB}}</ref> "
    "The explosion destroyed the rocket and the launch pad and caused millions of "
    "[[Dollar|dollars]] in damages; the event has also caused a big delay for [[NASA]] "
    "and [[Blue Origin]]."
    "<ref>{{Cite web|date=2026-05-29|title=Blue Origin New Glenn rocket explodes on "
    "launch pad in Florida - CBS News|"
    "url=https://www.cbsnews.com/news/blue-origin-new-glenn-rocket-explodes-launchpad-"
    "florida/|access-date=2026-06-02|website=www.cbsnews.com|language=en-US}}</ref>\n\n"
    "== Response ==\n"
    "Shortly after the incident, the founder of Blue Origin, [[Jeff Bezos]] made a "
    "statement on [[Twitter]] addressing the explosion, saying that all staff were safe."
    '<ref name="bbc">{{Cite news|date=2026-05-29|title=Blue Origin rocket explodes into '
    "huge ball of flame on Florida launch pad|website=BBC News|"
    "url=https://www.bbc.com/news/articles/cvgzl5wd8xeo|access-date=2026-05-29}}</ref>"
    "<ref>{{Cite web|last=Hogan|first=Brandon|date=2026-05-29|title=What is New Glenn, "
    "the Blue Origin rocket that exploded in Florida?|"
    "url=https://www.wjcl.com/article/florida-what-is-new-glenn-blue-origin-rocket-"
    "explosion/71438497|access-date=2026-05-29|website=wjcl}}</ref>\n\n"
    "== References ==\n<references />\n\n"
    "[[Category:2026 in the United States]]\n"
    "[[Category:Explosions in the United States]]\n"
    "[[Category:NASA]]"
)


class WikitextToProseTests(unittest.TestCase):
    def test_new_glenn_regression_keeps_full_prose(self):
        prose = wikitext_to_prose(NEW_GLENN_WIKITEXT)
        self.assertIn("New Glenn rocket blew up during testing", prose)
        self.assertIn("destroyed the rocket and the launch pad", prose)
        self.assertIn("Jeff Bezos", prose)
        self.assertGreater(len(prose), 200)

    def test_self_closing_ref_does_not_swallow_later_content(self):
        text = '<ref name=x /> keep this text <ref>real ref content</ref> more text'
        cleaned = wikitext_to_prose(text)
        self.assertIn("keep this text", cleaned)
        self.assertIn("more text", cleaned)

    def test_strip_templates_leaves_unmatched_opener_as_literal(self):
        text = "before {{unclosed template stays open after {{nested}} still no close"
        cleaned = strip_templates(text)
        self.assertIn("before {{unclosed template stays open after", cleaned)

    def test_strip_templates_removes_nested_balanced_template(self):
        text = "keep {{outer {{inner}} more}} tail"
        cleaned = strip_templates(text)
        self.assertEqual(cleaned, "keep  tail")


if __name__ == "__main__":
    unittest.main()
