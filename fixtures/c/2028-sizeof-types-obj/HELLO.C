int main(void) {
  int a = sizeof(int);
  int b = sizeof(long);
  int c = sizeof(char);
  int d = sizeof(int *);
  return a * 1000 + b * 100 + c * 10 + d;
}
