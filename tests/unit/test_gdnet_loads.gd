extends TestCase
## WP0: the gdnet extension loads and exposes NetVersion.


func test_netversion_class_is_registered() -> void:
	assert_true(ClassDB.class_exists("NetVersion"), "the gdnet extension did not load: NetVersion is missing")


func test_netversion_reports_a_semver_string() -> void:
	if not ClassDB.class_exists("NetVersion"):
		fail("the gdnet extension did not load")
		return
	var v: Variant = ClassDB.instantiate("NetVersion")
	var text: String = v.version()
	var parts := text.split(".")
	assert_eq(parts.size(), 3, "version should be MAJOR.MINOR.PATCH, got %s" % text)
	for part in parts:
		assert_true(part.is_valid_int(), "version part %s is not a number" % part)


func test_netcore_version_matches_the_extension() -> void:
	if not ClassDB.class_exists("NetVersion"):
		fail("the gdnet extension did not load")
		return
	var v: Variant = ClassDB.instantiate("NetVersion")
	assert_eq(v.version(), v.core_version(), "gdnet and netcore versions differ")
