int fn_void(void) { return 1; }
int fn_empty() { return 2; }
int main(void) {
  return fn_void() + fn_empty();
}
