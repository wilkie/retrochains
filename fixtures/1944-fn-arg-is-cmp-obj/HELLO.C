int print(int b) { return b ? 1 : 0; }
int main(void) {
  int a = 5;
  int c = 5;
  return print(a == c);
}
