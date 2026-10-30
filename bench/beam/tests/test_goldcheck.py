import goldcheck


def test_goldcheck_splits_a_page_three_ways():
    page = [{"kind": "fact", "content": "sprint: ends March 29"},
            {"kind": "turn", "content": "user: the budget is 4,500 dollars"}]
    assert goldcheck.where("The sprint ends on March 29.", page) == "memory"
    assert goldcheck.where("A budget of 4,500", page) == "turn"
    assert goldcheck.where("Kubernetes migration finished", page) == "nowhere"
    rows = [{"category": "x", "goldAnswers": ["March 29"], "recall": {"results": page}}]
    assert goldcheck.check(rows)["x"]["memory"] == 1
