/* Module 1: main() calls an extern function defined in MOD2. */
extern int helper(int n);

int main(void)
{
    return helper(7);
}
