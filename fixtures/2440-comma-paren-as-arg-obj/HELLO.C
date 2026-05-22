int single(int x) { return x * 10; }
int main(void) {
  int a;
  a = 0;
  return single((a = 5, a + 2));
}
