int helper(int x) { return x * 2; }
int wrapper(int x) { return helper(x); }
int main(void) {
  return wrapper(21);
}
