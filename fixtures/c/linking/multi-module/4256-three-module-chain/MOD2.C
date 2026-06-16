/* Module 2: b -> c (in MOD3). Mid-link in a three-OBJ chain. */
extern int c(int x);

int b(int x)
{
    return c(x) + 1;
}
