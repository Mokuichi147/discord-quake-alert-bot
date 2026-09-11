import unittest

from audit_p2p_points import audit


class AuditTests(unittest.TestCase):
    def test_area_and_station_names_are_counted_separately(self):
        reports = [
            {"issue": {"type": "ScalePrompt"}, "points": [
                {"pref": "東京", "addr": "東京都２３区", "isArea": True},
            ]},
            {"issue": {"type": "DetailScale"}, "points": [
                {"pref": "東京都", "addr": "東京都23区", "isArea": False},
                {"pref": "東京都", "addr": "東京都23区", "isArea": False},
                {"pref": "千葉県", "addr": "存在しない地点", "isArea": False},
            ]},
        ]
        summary = audit(reports, {("東京都", "東京都23区")})
        self.assertEqual(summary["ScalePrompt"]["is_area"], 1)
        self.assertEqual(summary["ScalePrompt"]["area_unique"], 1)
        self.assertEqual(summary["ScalePrompt"]["missing_unique"], 0)
        self.assertEqual(summary["ScalePrompt"]["matched_unique"], 0)
        self.assertEqual(summary["DetailScale"]["matched_unique"], 1)
        self.assertEqual(summary["DetailScale"]["missing_unique"], 1)


if __name__ == "__main__":
    unittest.main()
