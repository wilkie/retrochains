int id(int x) { return x; }
int main(void) {
  int i = 5; int j = 5;
  int a = id(++i);
  int b = id(j++);
  return a + b + i + j;
}
