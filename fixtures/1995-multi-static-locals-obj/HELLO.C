int next_id(void) {
  static int id = 0;
  static int count = 0;
  count++;
  return ++id;
}
int main(void) {
  int a = next_id();
  int b = next_id();
  return a + b;
}
