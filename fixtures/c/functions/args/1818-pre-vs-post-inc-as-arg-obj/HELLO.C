int identity(int x) { return x; }
int main(void) {
  int i = 5;
  int j = 5;
  int a = identity(++i);
  int b = identity(j++);
  return a * 100 + b + i * 10 + j;
}
