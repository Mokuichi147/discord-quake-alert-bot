"""観測点座標データ更新時の入力検証テスト。"""
import unittest

from update_observation_stations import convert


class ConversionTests(unittest.TestCase):
    def station(self, **changes):
        return dict(dict(pref="1", affi="0", name="観測点", lat="43.17", lon="141.32"), **changes)

    def test_uses_public_json_coordinates(self):
        rows = convert([self.station(lat="43.17167", lon="141.315")])
        self.assertEqual(rows, ["北海道\t観測点\t43.17167\t141.315"])
        rows = convert([self.station(lat="43.17", lon="141.32")])
        self.assertEqual(rows, ["北海道\t観測点\t43.17\t141.32"])

    def test_formats_binary_float_without_artifacts(self):
        rows = convert([self.station(lat="35.69", lon="139.70")])
        self.assertEqual(rows, ["北海道\t観測点\t35.69\t139.7"])

    def test_invalid_coordinates_and_keys_are_rejected(self):
        for changes in [dict(lat="nan"), dict(lon="inf"), dict(lat="91"),
                        dict(lon="181"), dict(pref="0"), dict(pref="48"),
                        dict(affi="3"), dict(name="地点\t名前")]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                convert([self.station(**changes)])
        with self.assertRaises(ValueError):
            convert([self.station(), self.station()])
        with self.assertRaises(ValueError):
            convert([])

    def test_same_name_in_different_prefectures_is_kept(self):
        rows = convert([self.station(pref="2"), self.station()])
        self.assertEqual(len(rows), 2)
        self.assertTrue(rows[0].startswith("北海道\t"))
        self.assertTrue(rows[1].startswith("青森県\t"))


if __name__ == "__main__":
    unittest.main()
