# Fixture for the far side of the stamp-only applicable set — both far sides.
#
# KIND. cmake declares BOTH function_def and macro_def its function kinds, so
# every function enricher examines a macro_def — and its kind_map sends the row
# to fql_kind = 'macro', outside the set the field table declares. The macro
# below is therefore a row that WAS examined and still answers neither value.
#
# LANGUAGE. cmake declares no comment kind, so the marker scan returns before
# reading anything here; the shadow enricher reads no language capability and
# does run. So the FUNCTION below answers has_shadow = 'false' and answers
# neither value for has_todo — one row, two answers, decided by the language
# rather than by the kind.

function(examined_and_inside)
    set(value 1)
    message(STATUS "${value}")
endfunction()

macro(examined_and_outside)
    set(value 1)
    message(STATUS "${value}")
endmacro()
