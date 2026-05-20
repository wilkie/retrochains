void swap(int *a, int *b) {
  int t;
  t = *a;
  *a = *b;
  *b = t;
}
int main(void) {
  int x = 1;
  int y = 2;
  swap(&x, &y);
  return x;
}
