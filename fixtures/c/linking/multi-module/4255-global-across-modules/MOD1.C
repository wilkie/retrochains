/* Module 1: reads and writes a global defined in MOD2 (extern variable). */
extern int counter;

int main(void)
{
    counter = counter + 5;
    return counter;
}
