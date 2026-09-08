"""What a collection does with ordinary documents.

These are the calls the old suite printed and never checked: find with each
operator, update_many, delete_many, create_index, fuzzy_search and lazy. Each
one asserts the answer, not that the call returned.
"""

import unittest

from harness import DBTestCase


PEOPLE = [
    {"name": "Ada", "age": 36, "city": "London", "active": True},
    {"name": "Alan", "age": 41, "city": "London", "active": True},
    {"name": "Grace", "age": 45, "city": "New York", "active": False},
    {"name": "Katherine", "age": 52, "city": "New York", "active": True},
]


class InsertAndRead(DBTestCase):
    def setUp(self):
        self.table, self.people = self.new_collection()
        self.people.insert_many(PEOPLE)

    def test_insert_many_returns_an_id_per_document(self):
        _, other = self.new_collection()
        ids = other.insert_many(PEOPLE)
        self.assertEqual(len(ids), len(PEOPLE))
        self.assertEqual(len(set(ids)), len(PEOPLE), "ids are not unique")

    def test_a_document_comes_back_with_the_fields_it_went_in_with(self):
        ada = self.people.find_one({"name": "Ada"})
        self.assertIsNotNone(ada)
        self.assertEqual(ada["age"], 36)
        self.assertEqual(ada["city"], "London")
        self.assertIs(ada["active"], True)
        self.assertIn("created_at", ada)

    def test_find_with_no_query_returns_everything(self):
        self.assertEqual(len(self.people.find()), len(PEOPLE))
        self.assertEqual(self.people.count(), len(PEOPLE))
        self.assertEqual(len(self.people), len(PEOPLE))

    def test_find_one_on_no_match_is_none_not_an_error(self):
        self.assertIsNone(self.people.find_one({"name": "Nobody"}))

    def test_a_later_document_may_carry_a_field_the_first_did_not(self):
        self.people.insert_one({"name": "Edsger", "age": 42, "country": "NL"})
        found = self.people.find_one({"name": "Edsger"})
        self.assertEqual(found["country"], "NL")
        # And the rows written before the column existed read as null.
        self.assertIsNone(self.people.find_one({"name": "Ada"})["country"])

    def test_a_nested_object_or_list_survives_the_round_trip(self):
        self.people.insert_one(
            {"name": "Barbara", "meta": {"team": "compilers"}, "tags": ["cobol", "navy"]}
        )
        found = self.people.find_one({"name": "Barbara"})
        self.assertEqual(found["meta"], {"team": "compilers"})
        self.assertEqual(found["tags"], ["cobol", "navy"])


class Operators(DBTestCase):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.people = cls.db.collection("operators")
        cls.people.insert_many(PEOPLE)

    def names(self, query):
        return sorted(d["name"] for d in self.people.find(query))

    def test_equality_is_the_default(self):
        self.assertEqual(self.names({"city": "London"}), ["Ada", "Alan"])

    def test_gt_gte_lt_lte(self):
        self.assertEqual(self.names({"age": {"$gt": 41}}), ["Grace", "Katherine"])
        self.assertEqual(self.names({"age": {"$gte": 41}}), ["Alan", "Grace", "Katherine"])
        self.assertEqual(self.names({"age": {"$lt": 41}}), ["Ada"])
        self.assertEqual(self.names({"age": {"$lte": 41}}), ["Ada", "Alan"])

    def test_eq_and_ne(self):
        self.assertEqual(self.names({"name": {"$eq": "Ada"}}), ["Ada"])
        self.assertEqual(self.names({"city": {"$ne": "London"}}), ["Grace", "Katherine"])

    def test_in_and_nin(self):
        self.assertEqual(self.names({"name": {"$in": ["Ada", "Grace"]}}), ["Ada", "Grace"])
        self.assertEqual(
            self.names({"name": {"$nin": ["Ada", "Grace"]}}), ["Alan", "Katherine"]
        )

    def test_in_with_an_empty_list_matches_nothing(self):
        self.assertEqual(self.names({"name": {"$in": []}}), [])

    def test_exists(self):
        self.assertEqual(len(self.people.find({"city": {"$exists": True}})), 4)
        self.assertEqual(len(self.people.find({"city": {"$exists": False}})), 0)

    def test_regex_is_anchored_where_the_caller_anchors_it(self):
        self.assertEqual(self.names({"name": {"$regex": "^A"}}), ["Ada", "Alan"])
        self.assertEqual(self.names({"name": {"$regex": "ce$"}}), ["Grace"])

    def test_two_fields_are_and_ed(self):
        self.assertEqual(self.names({"city": "London", "age": {"$gt": 40}}), ["Alan"])

    def test_a_boolean_field_is_matched_as_a_boolean(self):
        self.assertEqual(self.names({"active": False}), ["Grace"])


class BadQueries(DBTestCase):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.people = cls.db.collection("badqueries")
        cls.people.insert_many(PEOPLE)

    def test_an_unknown_operator_is_an_error(self):
        with self.assertRaises(Exception) as caught:
            self.people.find({"age": {"$bogus": 1}})
        self.assertIn("$bogus", str(caught.exception))

    def test_in_with_a_string_instead_of_a_list_is_an_error(self):
        with self.assertRaises(Exception) as caught:
            self.people.find({"name": {"$in": "Ada"}})
        self.assertIn("$in", str(caught.exception))

    def test_exists_with_a_non_boolean_is_an_error(self):
        with self.assertRaises(Exception):
            self.people.find({"city": {"$exists": "yes"}})

    def test_an_update_without_set_is_an_error(self):
        with self.assertRaises(Exception):
            self.people.update_many({"city": "London"}, {"$inc": {"age": 1}})

    def test_an_unknown_index_type_is_an_error(self):
        with self.assertRaises(Exception) as caught:
            self.people.create_index("name", "quadtree")
        self.assertIn("quadtree", str(caught.exception))


class Writes(DBTestCase):
    def setUp(self):
        self.table, self.people = self.new_collection()
        self.people.insert_many(PEOPLE)

    def test_update_one_touches_exactly_one_row(self):
        self.assertEqual(self.people.update_one({"city": "London"}, {"$set": {"age": 1}}), 1)
        self.assertEqual(self.people.count({"age": 1}), 1)

    def test_update_many_touches_every_match_and_says_how_many(self):
        self.assertEqual(
            self.people.update_many({"city": "London"}, {"$set": {"city": "Cambridge"}}), 2
        )
        self.assertEqual(self.people.count({"city": "Cambridge"}), 2)
        self.assertEqual(self.people.count({"city": "London"}), 0)

    def test_update_may_introduce_a_field_the_collection_has_never_seen(self):
        # insert_one grows the schema, so $set has to as well. It used to fail
        # here with `column "country" does not exist`.
        self.assertEqual(
            self.people.update_many({"city": "London"}, {"$set": {"country": "UK"}}), 2
        )
        self.assertEqual(self.people.count({"country": "UK"}), 2)
        self.assertIsNone(self.people.find_one({"name": "Grace"})["country"])

    def test_update_matching_nothing_changes_nothing(self):
        self.assertEqual(self.people.update_many({"city": "Mars"}, {"$set": {"age": 0}}), 0)
        self.assertEqual(self.people.update_one({"city": "Mars"}, {"$set": {"age": 0}}), 0)
        self.assertEqual(self.people.count(), len(PEOPLE))

    def test_delete_one_removes_one(self):
        self.assertEqual(self.people.delete_one({"city": "London"}), 1)
        self.assertEqual(self.people.count({"city": "London"}), 1)

    def test_delete_many_removes_every_match(self):
        self.assertEqual(self.people.delete_many({"city": "London"}), 2)
        self.assertEqual(self.people.count(), 2)
        self.assertNotIn("Ada", [d["name"] for d in self.people.find()])

    def test_delete_many_with_no_query_empties_the_collection(self):
        self.assertEqual(self.people.delete_many(), len(PEOPLE))
        self.assertEqual(self.people.count(), 0)


class Indexes(DBTestCase):
    def setUp(self):
        self.table, self.people = self.new_collection()
        self.people.insert_many(PEOPLE)

    def index_names(self, table):
        rows = self.db.query(
            "SELECT indexname FROM pg_indexes WHERE tablename = $1", [table]
        )
        return {r["indexname"] for r in rows}

    def test_a_btree_index_is_actually_created(self):
        self.people.create_index("age", "btree")
        self.assertIn(f"idx_{self.table}_age_btree", self.index_names(self.table))

    def test_a_gin_index_on_a_text_field_is_created_with_a_trigram_opclass(self):
        # Without an operator class PostgreSQL rejects GIN over text outright,
        # which is what this call used to do.
        self.people.create_index("name", "gin")
        self.assertIn(f"idx_{self.table}_name_gin", self.index_names(self.table))

    def test_creating_the_same_index_twice_is_not_an_error(self):
        self.people.create_index("age", "btree")
        self.people.create_index("age", "btree")


class FuzzySearch(DBTestCase):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.people = cls.db.collection("fuzzy")
        cls.people.insert_many(PEOPLE)

    def test_a_misspelling_still_finds_the_row(self):
        hits = self.people.fuzzy_search("Katharine", "name")
        self.assertEqual([h["name"] for h in hits], ["Katherine"])

    def test_every_hit_carries_its_similarity(self):
        hits = self.people.fuzzy_search("Ada", "name")
        self.assertTrue(hits, "an exact spelling should match itself")
        for hit in hits:
            self.assertGreater(hit["similarity"], 0.0)
            self.assertLessEqual(hit["similarity"], 1.0)

    def test_a_high_threshold_rejects_a_loose_match(self):
        loose = self.people.fuzzy_search("Katharin", "name", None, 0.3)
        strict = self.people.fuzzy_search("Katharin", "name", None, 0.95)
        self.assertTrue(loose)
        self.assertEqual(strict, [])

    def test_similarity_is_1_for_identical_strings_and_lower_otherwise(self):
        self.assertAlmostEqual(self.people.similarity("Ada", "Ada"), 1.0, places=5)
        self.assertLess(self.people.similarity("Ada", "Grace"), 0.5)

    def test_an_unknown_mode_is_an_error(self):
        with self.assertRaises(Exception) as caught:
            self.people.fuzzy_search("Ada", "name", "sideways")
        self.assertIn("sideways", str(caught.exception))

    def test_a_threshold_outside_0_to_1_is_an_error(self):
        with self.assertRaises(Exception):
            self.people.fuzzy_search("Ada", "name", None, 1.5)


class Lazy(DBTestCase):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.people = cls.db.collection("lazyframe")
        cls.people.insert_many(PEOPLE)

    def test_filter_and_limit(self):
        rows = self.people.lazy().filter("age > 40").collect()
        self.assertEqual(sorted(r["name"] for r in rows), ["Alan", "Grace", "Katherine"])
        self.assertEqual(len(self.people.lazy().limit(2).collect()), 2)

    def test_select_returns_only_the_named_columns(self):
        rows = self.people.lazy().select(["name", "age"]).collect()
        self.assertEqual(set(rows[0].keys()), {"name", "age"})

    def test_order_by_and_offset(self):
        ordered = self.people.lazy().select(["name"]).order_by("age ASC").collect()
        self.assertEqual([r["name"] for r in ordered], ["Ada", "Alan", "Grace", "Katherine"])
        skipped = self.people.lazy().select(["name"]).order_by("age ASC").offset(2).collect()
        self.assertEqual([r["name"] for r in skipped], ["Grace", "Katherine"])

    def test_the_steps_compose(self):
        rows = (
            self.people.lazy()
            .filter("city = 'New York'")
            .select(["name", "age"])
            .order_by("age DESC")
            .limit(1)
            .collect()
        )
        self.assertEqual(rows, [{"name": "Katherine", "age": 52}])


class Database(DBTestCase):
    def test_list_collections_sees_what_was_created(self):
        self.db.collection("visible").insert_one({"a": 1})
        self.assertIn("visible", self.db.list_collections())

    def test_drop_collection_removes_it(self):
        self.db.collection("temporary").insert_one({"a": 1})
        self.db.drop_collection("temporary")
        self.assertNotIn("temporary", self.db.list_collections())

    def test_opening_a_live_path_twice_fails_with_a_usable_message(self):
        # One process cannot hold two handles on the same database: the second
        # one would own a watcher that shuts the first one's server down. The
        # error says so, and says what to call instead.
        import dbcenter

        with self.assertRaises(Exception) as caught:
            dbcenter.DBCenter(path=self.db_path, persist=True)
        message = str(caught.exception)
        self.assertIn("already running", message)
        self.assertIn("force_shutdown", message)

    def test_query_takes_parameters_and_returns_dicts(self):
        rows = self.db.query("SELECT $1::int AS n, $2::text AS s", [7, "seven"])
        self.assertEqual(rows, [{"n": 7, "s": "seven"}])

    def test_the_port_is_a_real_port_and_postgres_answers_on_it(self):
        # Reading this over a Unix socket used to raise: inet_server_port() is
        # NULL there, which is precisely when the caller needs to be told.
        self.assertGreater(self.db.port, 0)
        self.assertEqual(self.db.query("SELECT 1 AS one")[0]["one"], 1)


if __name__ == "__main__":
    unittest.main()
