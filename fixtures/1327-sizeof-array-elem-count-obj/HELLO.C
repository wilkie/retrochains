int a[5];
int main(void) {
  int n = sizeof(a) / sizeof(a[0]);
  return n;
}
