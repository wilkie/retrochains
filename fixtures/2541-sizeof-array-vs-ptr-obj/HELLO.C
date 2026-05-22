int arr[5];
int main(void) {
  int *p;
  p = arr;
  return sizeof(arr) + sizeof(p);
}
