int add(int a, int b) { return a + b; }
int main(void) {
  int (*op)(int, int) = add;
  return op(3, 4);
}
