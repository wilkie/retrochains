int worker(int n) { return n + 1; }
extern int helper(int);
int main(void) { return helper(worker(5)); }
