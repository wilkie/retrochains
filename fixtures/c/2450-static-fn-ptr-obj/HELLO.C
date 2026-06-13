int adder(int x) { return x + 100; }
static int (*fp)(int) = adder;
int main(void) {
  return fp(5);
}
