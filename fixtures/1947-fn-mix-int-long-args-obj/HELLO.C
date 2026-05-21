int mix(int a, long b, int c) {
  return a + (int)b + c;
}
int main(void) {
  return mix(1, 100L, 1000);
}
