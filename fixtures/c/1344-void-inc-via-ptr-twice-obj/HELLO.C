void inc(int *p) {
  (*p)++;
}
int main(void) {
  int x = 5;
  inc(&x);
  inc(&x);
  return x;
}
