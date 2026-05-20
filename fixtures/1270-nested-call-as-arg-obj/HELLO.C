int add(int a, int b) {
  return a + b;
}
int main(void) {
  return add(add(1, 2), 3);
}
