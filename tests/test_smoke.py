import text_processing_engine


def test_main_runs(capsys):
    text_processing_engine.main()
    assert "text-processing-engine" in capsys.readouterr().out
