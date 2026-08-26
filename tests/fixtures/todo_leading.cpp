// Marker comments in the leading position of a function body, under the C++
// grammar rather than the C one — the two are separate plugins and the
// subscript exemption has already shown they can disagree on where a child
// sits.

namespace todo_leading {

int leadingMarker()
{
    // TODO: opens the body
    return 1;
}

int markerAfterStatement()
{
    int value = 1;
    // TODO: the position that always worked
    return value;
}

int markerBeforeTheBody() // TODO: between the signature and the body
{
    return 2;
}

int noMarker()
{
    return 3;
}

}  // namespace todo_leading
