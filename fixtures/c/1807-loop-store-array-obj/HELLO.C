int main(void) {
  int a[5];
  int i;
  int sum = 0;
  for (i = 0; i < 5; i++) a[i] = i * i;
  for (i = 0; i < 5; i++) sum += a[i];
  return sum;
}
