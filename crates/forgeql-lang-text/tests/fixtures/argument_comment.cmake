# A call whose parentheses also enclose a comment. The comment is a sibling
# of the arguments inside argument_list, never part of an argument.
zephyr_sources_ifdef(CONFIG_ENCLOSED_FLAG
  enclosed_source.c # ONLYINCOMMENT
)
