int main(void) {
  int a[10];
  int i;
  int sum = 0;
  for (i = 0; i < 10; i++) a[i] = i + 1;
  for (i = 0; i < 10; i++) sum += a[i];
  return sum;
}
