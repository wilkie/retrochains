enum { N = 10 };
int main(void) {
  int arr[N];
  int i;
  int sum;
  i = 0;
  sum = 0;
  while (i < N) {
    arr[i] = i + 1;
    sum = sum + arr[i];
    i = i + 1;
  }
  return sum;
}
