extern int helper(int);
int worker(int n) { return n + 1; }
int main(void) { return helper(worker(5)); }
