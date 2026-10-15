"""An app module that does not import: a dependency is missing."""
import a_dependency_that_is_not_installed  # noqa: F401

from webcortex import WebCortex

app = WebCortex("unimportable")
