int arr[5];
int *p1(void) { return arr; }
int *p2(void) { return &arr[0]; }
