#!/usr/bin/env python3
"""
test_capture_real_corpus.py — regression tests for the wikitext cleanup helpers in
capture_real_corpus.py. Run with: python3 crates/core/scripts/test_capture_real_corpus.py

Not part of the cargo test / CI gate (capture_real_corpus.py itself is an offline,
outside-CI tool — see its module docstring) — this is a standalone stdlib-only
`unittest` file for a maintainer to run locally when touching the cleanup regexes.
"""

import json
import os
import tempfile
import unittest
from unittest import mock

import capture_real_corpus
from capture_real_corpus import (
    derive_prose_from_wal,
    derive_relation_type_samples_from_wal,
    read_corpus_prose,
    stage_corpus,
    strip_file_links,
    strip_templates,
    wikitext_to_prose,
    write_corpus_prose,
)

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


# Simple English Wikipedia, "Aryabhata (satellite)" (rev 10314108), fetched verbatim. Its
# infobox caption `[[File:1984 CPA 5493.jpg|thumb|right|1984 USSR stamp featuring
# [[Bhaskara (satellite)|Bhaskara]]-I, Bhaskara-II and Aryabhata satellites]]` nests a piped
# wikilink inside the file-link caption. The old `_FILE_LINK_RE`
# (`\[\[(File|Image|Category):[^\]]*\]\]`) stopped at the *first* `]]` — the inner link's
# closer, not the file link's own — leaving `-I, Bhaskara-II and Aryabhata satellites]]` as
# debris in the prose. This is also a genuine stub article (154 chars of real prose), which
# previously tripped the short-prose guard into a hard abort instead of a skip. See #217.
ARYABHATA_WIKITEXT = (
    "{{Infobox spaceflight\n"
    "| name                  = Aryabhatta\n"
    "| image                 = Aryabhata Satellite.jpg\n"
    "| image_size            = 270px\n"
    "| image_caption         = File photo of Aryabhata, India's first indigenously built "
    "satellite.\n"
    "| mission_type          = [[Astrophysics]]\n"
    "| operator              = [[Indian Space Research Organisation|ISRO]]\n"
    "| website               =\n"
    "| COSPAR_ID             = 1965-033A\n"
    "| SATCAT                = 7752\n"
    "| mission_duration      = 4&nbsp;days achieved\n"
    "| spacecraft_bus        =\n"
    "| manufacturer          =\n"
    "| dry_mass              =\n"
    "| launch_mass           = 360&nbsp;kg (794&nbsp;lb)\n"
    "| power                 = 46&nbsp;watts\n"
    "| launch_date           ={{start-date|19 April 1975, 07:30|timezone=yes}}&nbsp;UTC"
    '<ref name="launchlog">{{cite web|url=http://planet4589.org/space/log/launchlog.txt|'
    "title=Launch Log|first=Jonathan|last=McDowell|work=Jonathan's Space Page|"
    "access-date=22 January 2014}}</ref>\n"
    "| launch_rocket         = [[Kosmos-3M]]\n"
    "| launch_site           = [[Kapustin Yar]] [[Kapustin Yar Site 107|107/2]]\n"
    "| launch_contractor     =\n"
    "| last_contact          = {{end-date|24 April 1975}}\n"
    "| decay_date            = 10 February 1992\n"
    '| orbit_epoch           = 19 May 1975<ref name="satcat">{{cite web|'
    "url=http://planet4589.org/space/log/satcat.txt|title=Satellite Catalog|"
    "first=Jonathan|last=McDowell|work=Jonathan's Space Page|"
    "access-date=22 January 2014}}</ref>\n"
    "| orbit_reference       = [[geocentric orbit|Geocentric]]\n"
    "| orbit_regime          = [[Low Earth orbit|Low Earth]]\n"
    "| orbit_periapsis       = {{convert|568|km|mi}}\n"
    "| orbit_apoapsis        = {{convert|611|km|mi}}\n"
    "| orbit_inclination     = 50.6&nbsp;degrees\n"
    "| orbit_period          = 96.46&nbsp;minutes\n"
    "| apsis                 = gee\n"
    "}}\n"
    "[[File:1984 CPA 5493.jpg|thumb|right|1984 USSR stamp featuring "
    "[[Bhaskara (satellite)|Bhaskara]]-I, Bhaskara-II and Aryabhata satellites]]\n"
    "'''Aryabhata''' was [[India]]'s first [[satellite (artificial)|satellite]]. It got its "
    "name from the Indian [[astronomer]] of the same name."
    '<ref name="ref1">{{cite web|url=http://www.isro.org/satellites/aryabhata.aspx |'
    "title=Aryabhata - The first indigenously built satellite}}</ref>\n\n"
    "== References ==\n{{reflist}}\n\n{{multistub|sci|Asia}}\n\n"
    "[[Category:Satellites]]\n"
    "[[Category:Indian Space Research Organisation]]\n"
    "[[Category:Spacecraft launched in the 1970s]]"
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

    def test_aryabhata_regression_no_caption_debris(self):
        prose = wikitext_to_prose(ARYABHATA_WIKITEXT)
        self.assertNotIn("Bhaskara-II and Aryabhata satellites]]", prose)
        self.assertIn(
            "Aryabhata was India's first satellite. It got its name from the Indian "
            "astronomer of the same name.",
            prose,
        )

    def test_strip_file_links_removes_nested_link_in_caption(self):
        text = (
            "before [[File:x.jpg|thumb|the [[Bhaskara (satellite)|Bhaskara]]-I and "
            "Aryabhata satellites]] after"
        )
        cleaned = strip_file_links(text)
        self.assertEqual(cleaned, "before  after")

    def test_strip_file_links_leaves_unmatched_opener_as_literal(self):
        text = "before [[File:unclosed caption stays open"
        cleaned = strip_file_links(text)
        self.assertIn("before [[File:unclosed caption stays open", cleaned)

    def test_strip_file_links_ignores_plain_wikilink(self):
        text = "see [[Apollo 11]] for details"
        cleaned = strip_file_links(text)
        self.assertEqual(cleaned, text)


class CorpusProseTests(unittest.TestCase):
    """Covers the #217 follow-up: staging the cleaned prose fed to the extractor
    (`corpus_prose.jsonl`) as its own free, no-service phase before any ingest happens, since
    neither the WAL (post-extraction) nor a future LLM cassette (one model's exchange only)
    can serve as input for an extraction-model comparison (#228) — see the module docstring's
    "three fixture artifacts" table. Also covers the split of staging (Phase 1) from ingest
    (Phase 2): two capture attempts previously died mid-run on infrastructure faults after
    paying for real LLM extraction on 20-36 articles, discarding all of it — staging up front
    means Phase 2 depends on nothing but the local service and cannot fail on a Wikipedia blip.
    """

    def test_stage_corpus_fetches_and_cleans_each_article(self):
        # Aryabhata (satellite) is a genuine stub (<200 chars of real prose, see
        # WikitextToProseTests above) so stage_corpus correctly skips it as a stub rather
        # than staging it — this test exercises both outcomes over a two-article manifest.
        articles = [
            {"title": "2026 New Glenn rocket explosion", "revision_id": 10877681},
            {"title": "Aryabhata (satellite)", "revision_id": 10314108},
        ]
        wikitexts = {10877681: NEW_GLENN_WIKITEXT, 10314108: ARYABHATA_WIKITEXT}
        with mock.patch.object(
            capture_real_corpus, "fetch_wikitext", side_effect=lambda rev: wikitexts[rev]
        ):
            staged, skipped = stage_corpus(articles, wiki_delay=0)

        self.assertEqual(len(staged), 1)
        self.assertEqual(staged[0]["title"], "2026 New Glenn rocket explosion")
        self.assertEqual(staged[0]["revision_id"], 10877681)
        self.assertIn("New Glenn rocket blew up during testing", staged[0]["prose"])
        self.assertEqual(len(skipped), 1)
        self.assertEqual(skipped[0]["title"], "Aryabhata (satellite)")

    def test_stage_corpus_skips_short_prose_without_aborting(self):
        articles = [
            {"title": "2026 New Glenn rocket explosion", "revision_id": 10877681},
            {"title": "A Stub", "revision_id": 1},
        ]
        wikitexts = {10877681: NEW_GLENN_WIKITEXT, 1: "Too short."}
        with mock.patch.object(
            capture_real_corpus, "fetch_wikitext", side_effect=lambda rev: wikitexts[rev]
        ):
            staged, skipped = stage_corpus(articles, wiki_delay=0)

        self.assertEqual(len(staged), 1)
        self.assertEqual(len(skipped), 1)
        self.assertEqual(skipped[0]["title"], "A Stub")

    def test_write_and_read_corpus_prose_round_trip(self):
        staged = [{"title": "Apollo 11", "revision_id": 42, "prose": "Apollo 11 landed."}]
        skipped = [{"title": "A Stub", "revision_id": 1, "prose_chars": 10}]
        with tempfile.NamedTemporaryFile(suffix=".jsonl", mode="w") as tmp:
            write_corpus_prose(staged, skipped, tmp.name)
            with open(tmp.name, encoding="utf-8") as f:
                lines = [json.loads(line) for line in f]

            self.assertEqual(len(lines), 2)
            header, record = lines
            self.assertTrue(header["_header"])
            self.assertEqual(header["cleanup_version"], capture_real_corpus.CLEANUP_VERSION)
            self.assertEqual(header["record_count"], 1)
            self.assertEqual(header["skipped_articles"], skipped)
            self.assertIn("script_git_sha", header)
            self.assertEqual(record, staged[0])

            read_staged, read_skipped = read_corpus_prose(tmp.name)
        self.assertEqual(read_staged, staged)
        self.assertEqual(read_skipped, skipped)

    def test_read_corpus_prose_rejects_missing_header(self):
        with tempfile.NamedTemporaryFile(suffix=".jsonl", mode="w") as tmp:
            tmp.write(json.dumps({"title": "no header", "revision_id": 1, "prose": "x"}) + "\n")
            tmp.flush()
            with self.assertRaises(RuntimeError):
                read_corpus_prose(tmp.name)


class DeriveFromWalTests(unittest.TestCase):
    """Covers the #217 one-off backfill path: the WAL fixture actually committed was captured
    before the stage/ingest split existed, so its `corpus_prose.jsonl` and
    `expected_results.json.relation_type_samples` were derived after the fact directly from the
    already-captured WAL rather than from a staged file or a live `knowledge_list_relationships`
    call. See `derive_prose_from_wal` and `derive_relation_type_samples_from_wal`.
    """

    def _write_wal_dir(self, lines_by_file):
        tmpdir = tempfile.mkdtemp()
        for fname, lines in lines_by_file.items():
            with open(os.path.join(tmpdir, fname), "w", encoding="utf-8") as f:
                for line in lines:
                    f.write(json.dumps(line) + "\n")
        return tmpdir

    def test_derive_prose_from_wal_recovers_title_revision_and_prose(self):
        wal_dir = self._write_wal_dir(
            {
                "0000.jsonl": [
                    {
                        "cypher": "CREATE (:Episodic {uuid: $uuid, name: $name, ...})",
                        "params": {
                            "uuid": "ep-1",
                            "name": "Apollo 11",
                            "source": "text",
                            "source_description": "Simple English Wikipedia: Apollo 11 (rev 42)",
                            "content": "Apollo 11 was the first crewed Moon landing.",
                        },
                    },
                    {
                        "cypher": "CREATE (:Entity {uuid: $uuid, name: $name, ...})",
                        "params": {"uuid": "e-1", "name": "NASA"},
                    },
                ]
            }
        )
        records = derive_prose_from_wal(wal_dir)
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["title"], "Apollo 11")
        self.assertEqual(records[0]["revision_id"], 42)
        self.assertEqual(records[0]["prose"], "Apollo 11 was the first crewed Moon landing.")

    def test_derive_prose_from_wal_raises_on_unparseable_source_description(self):
        wal_dir = self._write_wal_dir(
            {
                "0000.jsonl": [
                    {
                        "cypher": "CREATE (:Episodic {uuid: $uuid, name: $name, ...})",
                        "params": {
                            "uuid": "ep-1",
                            "name": "Apollo 11",
                            "source_description": "not the expected format",
                            "content": "text",
                        },
                    }
                ]
            }
        )
        with self.assertRaises(RuntimeError):
            derive_prose_from_wal(wal_dir)

    def test_derive_relation_type_samples_from_wal_reads_relates_to_node_records(self):
        wal_dir = self._write_wal_dir(
            {
                "0000.jsonl": [
                    {
                        "cypher": "CREATE (:RelatesToNode_ {uuid: $uuid, fact: $fact, relation_type: $relation_type, ...})",
                        "params": {
                            "uuid": "rn-1",
                            "fact": "NASA launched Apollo 11",
                            "relation_type": "LAUNCHED",
                        },
                    },
                    {
                        "cypher": "CREATE (:RelatesToNode_ {uuid: $uuid, fact: $fact, relation_type: $relation_type, ...})",
                        "params": {"uuid": "rn-2", "fact": None, "relation_type": None},
                    },
                ]
            }
        )
        samples = derive_relation_type_samples_from_wal(wal_dir)
        self.assertEqual(len(samples), 1)
        self.assertEqual(samples[0]["uuid"], "rn-1")
        self.assertEqual(samples[0]["fact"], "NASA launched Apollo 11")
        self.assertEqual(samples[0]["relation_type"], "LAUNCHED")

    def test_derive_relation_type_samples_from_wal_respects_sample_size(self):
        lines = [
            {
                "cypher": "CREATE (:RelatesToNode_ {uuid: $uuid, fact: $fact, relation_type: $relation_type, ...})",
                "params": {"uuid": f"rn-{i}", "fact": f"fact {i}", "relation_type": "X"},
            }
            for i in range(5)
        ]
        wal_dir = self._write_wal_dir({"0000.jsonl": lines})
        samples = derive_relation_type_samples_from_wal(wal_dir, sample_size=2)
        self.assertEqual(len(samples), 2)


if __name__ == "__main__":
    unittest.main()
