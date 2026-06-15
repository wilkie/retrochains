int main(void) {
  int a = 0;
  int b = (a = 1, a + 2);
  return b;
}
