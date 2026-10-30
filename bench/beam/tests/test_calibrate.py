import calibrate


def row(qid, category, score, items=None):
    judge = {"method": "rubric", "items": [{"score": s} for s in items]} if items else {"method": "event_ordering"}
    return {"questionId": qid, "category": category, "score": score, "judge": judge}


def test_rejudge_agreement_and_delta():
    past = {"a": row("a", "abstention", 1.0, [1.0, 1.0]), "b": row("b", "event_ordering", 0.9)}
    ours = [row("a", "abstention", 0.75, [1.0, 0.5]), row("b", "event_ordering", 0.9)]
    published = {"categories": {"abstention": {"mean": 1.0}, "event_ordering": {"mean": 0.9}}}
    report = calibrate.compare("rejudge", ours, past, published)
    assert report["categories"]["abstention"]["agreement"] == 0.5
    assert report["categories"]["abstention"]["delta"] == -0.25
    assert report["categories"]["event_ordering"]["agreement"] == 1.0
    assert report["overall"]["agreement"] == 2 / 3
    assert "agreement" not in calibrate.compare("reanswer", ours, past, published)["overall"]
