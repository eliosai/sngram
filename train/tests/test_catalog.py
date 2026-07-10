from sngram_train.catalog import build_catalog


def test_catalog_assigns_every_physical_config_once():
    catalog = build_catalog(["Python", "Markdown", "Text", "1C_Enterprise"])

    assert catalog.configs == ("1C_Enterprise", "Markdown", "Python", "Text")
    assert catalog.format("core-programming/Python").config == "Python"
    assert catalog.format("docs-prose-markup/Markdown").config == "Markdown"
    assert catalog.format("long-tail/1C_Enterprise").config == "1C_Enterprise"


def test_text_is_scanned_once_and_routes_to_logical_formats():
    catalog = build_catalog(["Text"])

    assert catalog.configs == ("Text",)
    assert catalog.route("Text", {"path": "/README.txt", "extension": "txt"}) == (
        "docs-prose-markup/Text"
    )
    assert catalog.route("Text", {"path": "/cfg/app.json", "extension": "json"}) == (
        "config-build-infra/Text"
    )
    assert catalog.route("Text", {"path": "/data/items.csv", "extension": "csv"}) == (
        "data-query-schema/Text"
    )


def test_default_dataset_alias_is_not_treated_as_a_format():
    catalog = build_catalog(["default", "Rust"])

    assert catalog.configs == ("Rust",)
    assert [item.id for item in catalog.formats] == ["core-programming/Rust"]
