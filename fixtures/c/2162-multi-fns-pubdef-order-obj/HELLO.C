int a_fn(void) { return 1; }
int b_fn(void) { return 2; }
int c_fn(void) { return 3; }
int main(void) {
  return a_fn() + b_fn() + c_fn();
}
