/* Module 2: a second tentative definition of the same symbol.
   The linker merges the two communal definitions into one. */
int shared;

int touch(void)
{
    shared = shared + 2;
    return shared;
}
